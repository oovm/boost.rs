//! 块压缩纹理解码。
//!
//! 只提供把 BC1/BC7 块解码为 RGBA8 的入口；文件容器、mipmap 选择和资源命名
//! 由调用方决定。PNG 输出复用 [`crate::write_png`]。
//!
//! 两种格式都按规范在 crate 内实现，不依赖外部解码库。BC7 的分区表是
//! Khronos BPTC（`ARB_texture_compression_bptc`）规范数据，最高位标记 fix-up 像素。

/// 把 BC1（DXT1）块解码为 RGBA8。
///
/// `input` 必须至少包含覆盖 `width × height` 的完整块；不足时返回空缓冲。
pub fn decode_bc1(input: &[u8], width: u32, height: u32) -> Vec<u8> {
    decode_blocks(input, width, height, 8, decode_bc1_block)
}

/// 把 BC7 块解码为 RGBA8。
///
/// `input` 必须至少包含覆盖 `width × height` 的完整块；不足时返回空缓冲。
pub fn decode_bc7(input: &[u8], width: u32, height: u32) -> Vec<u8> {
    decode_blocks(input, width, height, 16, decode_bc7_block)
}

/// 按 4×4 块遍历输入，把每个块的 16 个 RGBA 纹素写回图像；边缘块只写有效像素。
fn decode_blocks<F>(input: &[u8], width: u32, height: u32, block_bytes: usize, mut decode: F) -> Vec<u8>
where
    F: FnMut(&[u8], &mut [[u8; 4]; 16]),
{
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let blocks_x = width.div_ceil(4);
    let blocks_y = height.div_ceil(4);
    let Some(required) = (blocks_x as usize)
        .checked_mul(blocks_y as usize)
        .and_then(|blocks| blocks.checked_mul(block_bytes))
    else {
        return Vec::new();
    };
    if input.len() < required {
        return Vec::new();
    }
    let Some(length) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return Vec::new();
    };

    let mut rgba = vec![0u8; length];
    for block_y in 0..blocks_y {
        for block_x in 0..blocks_x {
            let offset = ((block_y * blocks_x + block_x) as usize) * block_bytes;
            let mut texels = [[0u8; 4]; 16];
            decode(&input[offset..offset + block_bytes], &mut texels);
            for row in 0..4 {
                let y = block_y * 4 + row;
                if y >= height {
                    break;
                }
                for column in 0..4 {
                    let x = block_x * 4 + column;
                    if x >= width {
                        break;
                    }
                    let pixel = ((y as usize * width as usize) + x as usize) * 4;
                    rgba[pixel..pixel + 4].copy_from_slice(&texels[(row * 4 + column) as usize]);
                }
            }
        }
    }
    rgba
}

/// BC1 的调色板顺序是 c0、c1，再按 c0/c1 大小关系决定剩余两项。
fn decode_bc1_block(block: &[u8], texels: &mut [[u8; 4]; 16]) {
    let color0 = u16::from_le_bytes([block[0], block[1]]);
    let color1 = u16::from_le_bytes([block[2], block[3]]);
    let palette = bc1_palette(color0, color1);
    let indices = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    for (pixel, texel) in texels.iter_mut().enumerate() {
        let index = ((indices >> (2 * pixel)) & 0x03) as usize;
        *texel = palette[index];
    }
}

fn bc1_palette(color0: u16, color1: u16) -> [[u8; 4]; 4] {
    let unpack = |color: u16| {
        let red = ((color >> 11) & 0x1f) as u8;
        let green = ((color >> 5) & 0x3f) as u8;
        let blue = (color & 0x1f) as u8;
        [(red << 3) | (red >> 2), (green << 2) | (green >> 4), (blue << 3) | (blue >> 2)]
    };
    let first = unpack(color0);
    let second = unpack(color1);
    let mut palette = [[0u8; 4]; 4];
    palette[0] = [first[0], first[1], first[2], 255];
    palette[1] = [second[0], second[1], second[2], 255];
    if color0 > color1 {
        for channel in 0..3 {
            palette[2][channel] =
                ((2 * first[channel] as u16 + second[channel] as u16) / 3) as u8;
            palette[3][channel] =
                ((first[channel] as u16 + 2 * second[channel] as u16) / 3) as u8;
        }
        palette[2][3] = 255;
        palette[3][3] = 255;
    }
    else {
        for channel in 0..3 {
            palette[2][channel] = ((first[channel] as u16 + second[channel] as u16) / 2) as u8;
        }
        palette[2][3] = 255;
        // 4 色模式的第三项是不透明黑，第四项为全透明黑。
        palette[3] = [0, 0, 0, 0];
    }
    palette
}

/// BC7 每个模式的颜色分量位宽。
const COLOR_BITS: [u32; 8] = [4, 6, 5, 7, 5, 7, 7, 5];
/// BC7 每个模式的 alpha 分量位宽，`0` 表示该模式不存 alpha。
const ALPHA_BITS: [u32; 8] = [0, 0, 0, 0, 6, 8, 7, 5];
/// 使用 p-bit 的模式位图（0、1、3、6、7）。
const PBIT_MODES: u32 = 0b1100_1011;
/// 2 bit 索引的插值权重（满量程 64）。
const WEIGHT2: [u32; 4] = [0, 21, 43, 64];
/// 3 bit 索引的插值权重。
const WEIGHT3: [u32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
/// 4 bit 索引的插值权重。
const WEIGHT4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// BC7 块是 128 bit 的小端比特流，按需从低到高取位。
struct BitReader {
    low: u64,
    high: u64,
}

impl BitReader {
    fn new(block: &[u8]) -> Self {
        let mut low = [0u8; 8];
        low.copy_from_slice(&block[..8]);
        let mut high = [0u8; 8];
        high.copy_from_slice(&block[8..16]);
        Self { low: u64::from_le_bytes(low), high: u64::from_le_bytes(high) }
    }

    fn read(&mut self, bits: u32) -> u32 {
        if bits == 0 {
            return 0;
        }
        let mask = (1u64 << bits) - 1;
        let value = (self.low & mask) as u32;
        self.low = (self.low >> bits) | ((self.high & mask) << (64 - bits));
        self.high >>= bits;
        value
    }
}

fn decode_bc7_block(block: &[u8], texels: &mut [[u8; 4]; 16]) {
    let mut reader = BitReader::new(block);

    // 模式编码为「m 个 0 后跟一个 1」，最多消耗 8 bit。
    let mut mode = 0usize;
    while mode < 8 && reader.read(1) == 0 {
        mode += 1;
    }
    if mode >= 8 {
        // 保留模式：按规范输出全零（透明黑），texels 已是零值。
        return;
    }

    let (num_partitions, partition) = match mode {
        0 => (3, reader.read(4) as usize),
        1 | 3 | 7 => (2, reader.read(6) as usize),
        2 => (3, reader.read(6) as usize),
        _ => (1, 0),
    };

    let mut rotation = 0;
    let mut index_selection = 0;
    if mode == 4 || mode == 5 {
        rotation = reader.read(2);
        if mode == 4 {
            index_selection = reader.read(1);
        }
    }

    let color_bits = COLOR_BITS[mode];
    let alpha_bits = ALPHA_BITS[mode];
    let num_endpoints = num_partitions * 2;
    let mut endpoints = [[0u32; 4]; 6];

    // 端点按通道优先排列：先所有端点的 R，再 G、B，最后 alpha。
    for channel in 0..3 {
        for endpoint in endpoints.iter_mut().take(num_endpoints) {
            endpoint[channel] = reader.read(color_bits);
        }
    }
    if alpha_bits > 0 {
        for endpoint in endpoints.iter_mut().take(num_endpoints) {
            endpoint[3] = reader.read(alpha_bits);
        }
    }

    if PBIT_MODES & (1 << mode) != 0 {
        for endpoint in endpoints.iter_mut().take(num_endpoints) {
            for channel in 0..4 {
                endpoint[channel] <<= 1;
            }
        }
        if mode == 1 {
            // 模式 1 的两个子集各共享一个 p-bit。
            let first = reader.read(1);
            let second = reader.read(1);
            for channel in 0..3 {
                endpoints[0][channel] |= first;
                endpoints[1][channel] |= first;
                endpoints[2][channel] |= second;
                endpoints[3][channel] |= second;
            }
        }
        else {
            // 其余 p-bit 模式每个端点各有一个 p-bit。
            for endpoint in endpoints.iter_mut().take(num_endpoints) {
                let pbit = reader.read(1);
                for channel in 0..4 {
                    endpoint[channel] |= pbit;
                }
            }
        }
    }

    // 把端点左移到位宽最高位，再复制高位补齐低位。
    for endpoint in endpoints.iter_mut().take(num_endpoints) {
        let color_precision = color_bits + ((PBIT_MODES >> mode) & 1);
        for channel in 0..3 {
            let value = endpoint[channel] << (8 - color_precision);
            endpoint[channel] = value | (value >> color_precision);
        }
        let alpha_precision = alpha_bits + ((PBIT_MODES >> mode) & 1);
        let value = endpoint[3] << (8 - alpha_precision);
        endpoint[3] = value | (value >> alpha_precision);
    }
    if alpha_bits == 0 {
        for endpoint in endpoints.iter_mut().take(num_endpoints) {
            endpoint[3] = 255;
        }
    }

    let index_bits = match mode {
        0 | 1 => 3,
        6 => 4,
        _ => 2,
    };
    let index_bits2 = match mode {
        4 => 3,
        5 => 2,
        _ => 0,
    };
    let weights: &[u32] = match index_bits {
        2 => &WEIGHT2,
        3 => &WEIGHT3,
        _ => &WEIGHT4,
    };
    let weights2: &[u32] = if index_bits2 == 2 { &WEIGHT2 } else { &WEIGHT3 };

    let subset_of = |row: usize, column: usize| {
        if num_partitions == 1 {
            if row | column == 0 { 0x80 } else { 0 }
        }
        else if num_partitions == 2 {
            PARTITION_TWO[partition][row][column]
        }
        else {
            PARTITION_THREE[partition][row][column]
        }
    };

    // 第一遍收集颜色索引：fix-up 像素的索引少一位。
    let mut indices = [[0u32; 4]; 4];
    for row in 0..4 {
        for column in 0..4 {
            let bits = if subset_of(row, column) & 0x80 != 0 { index_bits - 1 } else { index_bits };
            indices[row][column] = reader.read(bits);
        }
    }

    // 第二遍读 alpha 索引（如有）、插值并应用通道旋转。
    for row in 0..4 {
        for column in 0..4 {
            let subset = (subset_of(row, column) & 0x03) as usize;
            let index = indices[row][column] as usize;
            let endpoint0 = endpoints[subset * 2];
            let endpoint1 = endpoints[subset * 2 + 1];
            let (red, green, blue, alpha);
            if index_bits2 == 0 {
                red = interpolate(endpoint0[0], endpoint1[0], weights, index);
                green = interpolate(endpoint0[1], endpoint1[1], weights, index);
                blue = interpolate(endpoint0[2], endpoint1[2], weights, index);
                alpha = interpolate(endpoint0[3], endpoint1[3], weights, index);
            }
            else {
                let bits = if row | column != 0 { index_bits2 } else { index_bits2 - 1 };
                let index2 = reader.read(bits) as usize;
                if index_selection == 0 {
                    red = interpolate(endpoint0[0], endpoint1[0], weights, index);
                    green = interpolate(endpoint0[1], endpoint1[1], weights, index);
                    blue = interpolate(endpoint0[2], endpoint1[2], weights, index);
                    alpha = interpolate(endpoint0[3], endpoint1[3], weights2, index2);
                }
                else {
                    red = interpolate(endpoint0[0], endpoint1[0], weights2, index2);
                    green = interpolate(endpoint0[1], endpoint1[1], weights2, index2);
                    blue = interpolate(endpoint0[2], endpoint1[2], weights2, index2);
                    alpha = interpolate(endpoint0[3], endpoint1[3], weights, index);
                }
            }

            let mut pixel = [red, green, blue, alpha];
            match rotation {
                1 => pixel.swap(3, 0),
                2 => pixel.swap(3, 1),
                3 => pixel.swap(3, 2),
                _ => {}
            }
            texels[row * 4 + column] = pixel;
        }
    }
}

fn interpolate(first: u32, second: u32, weights: &[u32], index: usize) -> u8 {
    let weight = weights[index];
    ((first * (64 - weight) + second * weight + 32) >> 6) as u8
}

/// 2 子集 BPTC 分区表，最高位标记该子集的 fix-up 像素。
const PARTITION_TWO: [[[u8; 4]; 4]; 64] = [
    [[128, 0, 1, 1], [0, 0, 1, 1], [0, 0, 1, 1], [0, 0, 1, 129]],
    [[128, 0, 0, 1], [0, 0, 0, 1], [0, 0, 0, 1], [0, 0, 0, 129]],
    [[128, 1, 1, 1], [0, 1, 1, 1], [0, 1, 1, 1], [0, 1, 1, 129]],
    [[128, 0, 0, 1], [0, 0, 1, 1], [0, 0, 1, 1], [0, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 1], [0, 0, 0, 1], [0, 0, 1, 129]],
    [[128, 0, 1, 1], [0, 1, 1, 1], [0, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 0, 1], [0, 0, 1, 1], [0, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 1], [0, 0, 1, 1], [0, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 1], [0, 0, 1, 129]],
    [[128, 0, 1, 1], [0, 1, 1, 1], [1, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 1], [0, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 1], [0, 1, 1, 129]],
    [[128, 0, 0, 1], [0, 1, 1, 1], [1, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [1, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 0, 0], [1, 1, 1, 1], [1, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0], [1, 1, 1, 129]],
    [[128, 0, 0, 0], [1, 0, 0, 0], [1, 1, 1, 0], [1, 1, 1, 129]],
    [[128, 1, 129, 1], [0, 0, 0, 1], [0, 0, 0, 0], [0, 0, 0, 0]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [129, 0, 0, 0], [1, 1, 1, 0]],
    [[128, 1, 129, 1], [0, 0, 1, 1], [0, 0, 0, 1], [0, 0, 0, 0]],
    [[128, 0, 129, 1], [0, 0, 0, 1], [0, 0, 0, 0], [0, 0, 0, 0]],
    [[128, 0, 0, 0], [1, 0, 0, 0], [129, 1, 0, 0], [1, 1, 1, 0]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [129, 0, 0, 0], [1, 1, 0, 0]],
    [[128, 1, 1, 1], [0, 0, 1, 1], [0, 0, 1, 1], [0, 0, 0, 129]],
    [[128, 0, 129, 1], [0, 0, 0, 1], [0, 0, 0, 1], [0, 0, 0, 0]],
    [[128, 0, 0, 0], [1, 0, 0, 0], [129, 0, 0, 0], [1, 1, 0, 0]],
    [[128, 1, 129, 0], [0, 1, 1, 0], [0, 1, 1, 0], [0, 1, 1, 0]],
    [[128, 0, 129, 1], [0, 1, 1, 0], [0, 1, 1, 0], [1, 1, 0, 0]],
    [[128, 0, 0, 1], [0, 1, 1, 1], [129, 1, 1, 0], [1, 0, 0, 0]],
    [[128, 0, 0, 0], [1, 1, 1, 1], [129, 1, 1, 1], [0, 0, 0, 0]],
    [[128, 1, 129, 1], [0, 0, 0, 1], [1, 0, 0, 0], [1, 1, 1, 0]],
    [[128, 0, 129, 1], [1, 0, 0, 1], [1, 0, 0, 1], [1, 1, 0, 0]],
    [[128, 1, 0, 1], [0, 1, 0, 1], [0, 1, 0, 1], [0, 1, 0, 129]],
    [[128, 0, 0, 0], [1, 1, 1, 1], [0, 0, 0, 0], [1, 1, 1, 129]],
    [[128, 1, 0, 1], [1, 0, 129, 0], [0, 1, 0, 1], [1, 0, 1, 0]],
    [[128, 0, 1, 1], [0, 0, 1, 1], [129, 1, 0, 0], [1, 1, 0, 0]],
    [[128, 0, 129, 1], [1, 1, 0, 0], [0, 0, 1, 1], [1, 1, 0, 0]],
    [[128, 1, 0, 1], [0, 1, 0, 1], [129, 0, 1, 0], [1, 0, 1, 0]],
    [[128, 1, 1, 0], [1, 0, 0, 1], [0, 1, 1, 0], [1, 0, 0, 129]],
    [[128, 1, 0, 1], [1, 0, 1, 0], [1, 0, 1, 0], [0, 1, 0, 129]],
    [[128, 1, 129, 1], [0, 0, 1, 1], [1, 1, 0, 0], [1, 1, 1, 0]],
    [[128, 0, 0, 1], [0, 0, 1, 1], [129, 1, 0, 0], [1, 0, 0, 0]],
    [[128, 0, 129, 1], [0, 0, 1, 0], [0, 1, 0, 0], [1, 1, 0, 0]],
    [[128, 0, 129, 1], [1, 0, 1, 1], [1, 1, 0, 1], [1, 1, 0, 0]],
    [[128, 1, 129, 0], [1, 0, 0, 1], [1, 0, 0, 1], [0, 1, 1, 0]],
    [[128, 0, 1, 1], [1, 1, 0, 0], [1, 1, 0, 0], [0, 0, 1, 129]],
    [[128, 1, 1, 0], [0, 1, 1, 0], [1, 0, 0, 1], [1, 0, 0, 129]],
    [[128, 0, 0, 0], [0, 1, 129, 0], [0, 1, 1, 0], [0, 0, 0, 0]],
    [[128, 1, 0, 0], [1, 1, 129, 0], [0, 1, 0, 0], [0, 0, 0, 0]],
    [[128, 0, 129, 0], [0, 1, 1, 1], [0, 0, 1, 0], [0, 0, 0, 0]],
    [[128, 0, 0, 0], [0, 0, 129, 0], [0, 1, 1, 1], [0, 0, 1, 0]],
    [[128, 0, 0, 0], [0, 1, 0, 0], [129, 1, 1, 0], [0, 1, 0, 0]],
    [[128, 1, 1, 0], [1, 1, 0, 0], [1, 0, 0, 1], [0, 0, 1, 129]],
    [[128, 0, 1, 1], [0, 1, 1, 0], [1, 1, 0, 0], [1, 0, 0, 129]],
    [[128, 1, 129, 0], [0, 0, 1, 1], [1, 0, 0, 1], [1, 1, 0, 0]],
    [[128, 0, 129, 1], [1, 0, 0, 1], [1, 1, 0, 0], [0, 1, 1, 0]],
    [[128, 1, 1, 0], [1, 1, 0, 0], [1, 1, 0, 0], [1, 0, 0, 129]],
    [[128, 1, 1, 0], [0, 0, 1, 1], [0, 0, 1, 1], [1, 0, 0, 129]],
    [[128, 1, 1, 1], [1, 1, 1, 0], [1, 0, 0, 0], [0, 0, 0, 129]],
    [[128, 0, 0, 1], [1, 0, 0, 0], [1, 1, 1, 0], [0, 1, 1, 129]],
    [[128, 0, 0, 0], [1, 1, 1, 1], [0, 0, 1, 1], [0, 0, 1, 129]],
    [[128, 0, 129, 1], [0, 0, 1, 1], [1, 1, 1, 1], [0, 0, 0, 0]],
    [[128, 0, 129, 0], [0, 0, 1, 0], [1, 1, 1, 0], [1, 1, 1, 0]],
    [[128, 1, 0, 0], [0, 1, 0, 0], [0, 1, 1, 1], [0, 1, 1, 129]],
];

/// 3 子集 BPTC 分区表，最高位标记对应子集的 fix-up 像素。
const PARTITION_THREE: [[[u8; 4]; 4]; 64] = [
    [[128, 0, 1, 129], [0, 0, 1, 1], [0, 2, 2, 1], [2, 2, 2, 130]],
    [[128, 0, 0, 129], [0, 0, 1, 1], [130, 2, 1, 1], [2, 2, 2, 1]],
    [[128, 0, 0, 0], [2, 0, 0, 1], [130, 2, 1, 1], [2, 2, 1, 129]],
    [[128, 2, 2, 130], [0, 0, 2, 2], [0, 0, 1, 1], [0, 1, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [129, 1, 2, 2], [1, 1, 2, 130]],
    [[128, 0, 1, 129], [0, 0, 1, 1], [0, 0, 2, 2], [0, 0, 2, 130]],
    [[128, 0, 2, 130], [0, 0, 2, 2], [1, 1, 1, 1], [1, 1, 1, 129]],
    [[128, 0, 1, 1], [0, 0, 1, 1], [130, 2, 1, 1], [2, 2, 1, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [129, 1, 1, 1], [2, 2, 2, 130]],
    [[128, 0, 0, 0], [1, 1, 1, 1], [129, 1, 1, 1], [2, 2, 2, 130]],
    [[128, 0, 0, 0], [1, 1, 129, 1], [2, 2, 2, 2], [2, 2, 2, 130]],
    [[128, 0, 1, 2], [0, 0, 129, 2], [0, 0, 1, 2], [0, 0, 1, 130]],
    [[128, 1, 1, 2], [0, 1, 129, 2], [0, 1, 1, 2], [0, 1, 1, 130]],
    [[128, 1, 2, 2], [0, 129, 2, 2], [0, 1, 2, 2], [0, 1, 2, 130]],
    [[128, 0, 1, 129], [0, 1, 1, 2], [1, 1, 2, 2], [1, 2, 2, 130]],
    [[128, 0, 1, 129], [2, 0, 0, 1], [130, 2, 0, 0], [2, 2, 2, 0]],
    [[128, 0, 0, 129], [0, 0, 1, 1], [0, 1, 1, 2], [1, 1, 2, 130]],
    [[128, 1, 1, 129], [0, 0, 1, 1], [130, 0, 0, 1], [2, 2, 0, 0]],
    [[128, 0, 0, 0], [1, 1, 2, 2], [129, 1, 2, 2], [1, 1, 2, 130]],
    [[128, 0, 2, 130], [0, 0, 2, 2], [0, 0, 2, 2], [1, 1, 1, 129]],
    [[128, 1, 1, 129], [0, 1, 1, 1], [0, 2, 2, 2], [0, 2, 2, 130]],
    [[128, 0, 0, 129], [0, 0, 0, 1], [130, 2, 2, 1], [2, 2, 2, 1]],
    [[128, 0, 0, 0], [0, 0, 129, 1], [0, 1, 2, 2], [0, 1, 2, 130]],
    [[128, 0, 0, 0], [1, 1, 0, 0], [130, 2, 129, 0], [2, 2, 1, 0]],
    [[128, 1, 2, 130], [0, 129, 2, 2], [0, 0, 1, 1], [0, 0, 0, 0]],
    [[128, 0, 1, 2], [0, 0, 1, 2], [129, 1, 2, 2], [2, 2, 2, 130]],
    [[128, 1, 1, 0], [1, 2, 130, 1], [129, 2, 2, 1], [0, 1, 1, 0]],
    [[128, 0, 0, 0], [0, 1, 129, 0], [1, 2, 130, 1], [1, 2, 2, 1]],
    [[128, 0, 2, 2], [1, 1, 0, 2], [129, 1, 0, 2], [0, 0, 2, 130]],
    [[128, 1, 1, 0], [0, 129, 1, 0], [2, 0, 0, 2], [2, 2, 2, 130]],
    [[128, 0, 1, 1], [0, 1, 2, 2], [0, 1, 130, 2], [0, 0, 1, 129]],
    [[128, 0, 0, 0], [2, 0, 0, 0], [130, 2, 1, 1], [2, 2, 2, 129]],
    [[128, 0, 0, 0], [0, 0, 0, 2], [129, 1, 2, 2], [1, 2, 2, 130]],
    [[128, 2, 2, 130], [0, 0, 2, 2], [0, 0, 1, 2], [0, 0, 1, 129]],
    [[128, 0, 1, 129], [0, 0, 1, 2], [0, 0, 2, 2], [0, 2, 2, 130]],
    [[128, 1, 2, 0], [0, 129, 2, 0], [0, 1, 130, 0], [0, 1, 2, 0]],
    [[128, 0, 0, 0], [1, 1, 129, 1], [2, 2, 130, 2], [0, 0, 0, 0]],
    [[128, 1, 2, 0], [1, 2, 0, 1], [130, 0, 129, 2], [0, 1, 2, 0]],
    [[128, 1, 2, 0], [2, 0, 1, 2], [129, 130, 0, 1], [0, 1, 2, 0]],
    [[128, 0, 1, 1], [2, 2, 0, 0], [1, 1, 130, 2], [0, 0, 1, 129]],
    [[128, 0, 1, 1], [1, 1, 130, 2], [2, 2, 0, 0], [0, 0, 1, 129]],
    [[128, 1, 0, 129], [0, 1, 0, 1], [2, 2, 2, 2], [2, 2, 2, 130]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [130, 1, 2, 1], [2, 1, 2, 129]],
    [[128, 0, 2, 2], [1, 129, 2, 2], [0, 0, 2, 2], [1, 1, 2, 130]],
    [[128, 0, 2, 130], [0, 0, 1, 1], [0, 0, 2, 2], [0, 0, 1, 129]],
    [[128, 2, 2, 0], [1, 2, 130, 1], [0, 2, 2, 0], [1, 2, 2, 129]],
    [[128, 1, 0, 1], [2, 2, 130, 2], [2, 2, 2, 2], [0, 1, 0, 129]],
    [[128, 0, 0, 0], [2, 1, 2, 1], [130, 1, 2, 1], [2, 1, 2, 129]],
    [[128, 1, 0, 129], [0, 1, 0, 1], [0, 1, 0, 1], [2, 2, 2, 130]],
    [[128, 2, 2, 130], [0, 1, 1, 1], [0, 2, 2, 2], [0, 1, 1, 129]],
    [[128, 0, 0, 2], [1, 129, 1, 2], [0, 0, 0, 2], [1, 1, 1, 130]],
    [[128, 0, 0, 0], [2, 129, 1, 2], [2, 1, 1, 2], [2, 1, 1, 130]],
    [[128, 2, 2, 2], [0, 129, 1, 1], [0, 1, 1, 1], [0, 2, 2, 130]],
    [[128, 0, 0, 2], [1, 1, 1, 2], [129, 1, 1, 2], [0, 0, 0, 130]],
    [[128, 1, 1, 0], [0, 129, 1, 0], [0, 1, 1, 0], [2, 2, 2, 130]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [2, 1, 129, 2], [2, 1, 1, 130]],
    [[128, 1, 1, 0], [0, 129, 1, 0], [2, 2, 2, 2], [2, 2, 2, 130]],
    [[128, 0, 2, 2], [0, 0, 1, 1], [0, 0, 129, 1], [0, 0, 2, 130]],
    [[128, 0, 2, 2], [1, 1, 2, 2], [129, 1, 2, 2], [0, 0, 2, 130]],
    [[128, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0], [2, 129, 1, 130]],
    [[128, 0, 0, 130], [0, 0, 0, 1], [0, 0, 0, 2], [0, 0, 0, 129]],
    [[128, 2, 2, 2], [1, 2, 2, 2], [0, 2, 2, 2], [129, 2, 2, 130]],
    [[128, 1, 0, 129], [2, 2, 2, 2], [2, 2, 2, 2], [2, 2, 2, 130]],
    [[128, 1, 1, 129], [2, 0, 1, 1], [130, 2, 0, 1], [2, 2, 2, 0]],
];

#[cfg(test)]
mod tests {
    use super::{decode_bc1, decode_bc7};

    #[test]
    fn bc1_constant_white_block_decodes_to_opaque_rgba() {
        // color0 = 0xffff (white), color1 = 0x0000, all indices select color0.
        let mut block = vec![0xff, 0xff, 0x00, 0x00];
        block.extend_from_slice(&[0u8; 4]);
        let rgba = decode_bc1(&block, 4, 4);
        assert_eq!(rgba.len(), 64);
        assert_eq!(&rgba[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn bc1_short_input_returns_empty() {
        assert!(decode_bc1(&[0u8; 7], 4, 4).is_empty());
    }

    #[test]
    fn bc7_reserved_mode_decodes_to_transparent_black() {
        // 8 个 0 bit 表示保留模式。
        assert_eq!(decode_bc7(&[0u8; 16], 4, 4), vec![0u8; 64]);
    }

    #[test]
    fn bc7_mode_6_constant_endpoints_decode_to_solid_colour() {
        // 模式 6：6 个 0 后跟 1；端点为 7 bit + p-bit，索引 4 bit。
        // 两个端点都写红色（R=127，p=0 → 254，G/B=0），全部索引为 0。
        let mut writer = BitWriter::new();
        writer.push(0, 6);
        writer.push(1, 1); // mode 6
        for _ in 0..2 {
            writer.push(127, 7); // R0 / R1
        }
        for _ in 0..2 {
            writer.push(0, 7); // G0 / G1
        }
        for _ in 0..2 {
            writer.push(0, 7); // B0 / B1
        }
        for _ in 0..2 {
            writer.push(127, 7); // A0 / A1
        }
        for _ in 0..2 {
            writer.push(0, 1); // p-bits
        }
        // 首像素是 fix-up 索引，只有 3 bit，其余 15 个像素各 4 bit。
        writer.push(0, 3);
        for _ in 0..15 {
            writer.push(0, 4);
        }
        let block = writer.finish();
        let rgba = decode_bc7(&block, 4, 4);
        assert_eq!(&rgba[0..4], &[254, 0, 0, 254]);
    }

    /// 测试用的低位优先比特写入器，与 [`BitReader`] 对应。
    struct BitWriter {
        bytes: Vec<u8>,
        bit: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bytes: vec![0u8; 16], bit: 0 }
        }

        fn push(&mut self, value: u32, bits: u32) {
            for offset in 0..bits {
                if value >> offset & 1 != 0 {
                    self.bytes[self.bit / 8] |= 1 << (self.bit % 8);
                }
                self.bit += 1;
            }
        }

        fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }
}
