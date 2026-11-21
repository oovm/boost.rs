//! 自有 AES 分组密码实现（FIPS 197）。
//!
//! 只提供单分组加解密，不承载模式、填充或认证：GCM 在 [`crate::aead`]，CBC 在
//! [`crate::cipher`]。两处共用同一套密钥编排与分组变换，crate 内不再保留第二份 AES。

use std::sync::OnceLock;

/// 展开后的 AES 密钥编排。
pub(crate) struct AesKey {
    round_keys: Vec<u8>,
    rounds: usize,
}

impl AesKey {
    /// 接受 16（AES-128）或 32（AES-256）字节密钥，其它长度返回 `None`。
    pub(crate) fn new(key: &[u8]) -> Option<Self> {
        let rounds = match key.len() {
            16 => 10,
            32 => 14,
            _ => return None,
        };
        Some(Self { round_keys: expand_key(key, rounds), rounds })
    }

    /// 原地加密一个 16 字节分组。
    pub(crate) fn encrypt_block(&self, block: &mut [u8; 16]) {
        let table = sbox();
        add_round_key(block, &self.round_keys[..16]);
        for round in 1..self.rounds {
            sub_bytes(block, table);
            shift_rows(block);
            mix_columns(block);
            add_round_key(block, &self.round_keys[round * 16..round * 16 + 16]);
        }
        sub_bytes(block, table);
        shift_rows(block);
        add_round_key(block, &self.round_keys[self.rounds * 16..self.rounds * 16 + 16]);
    }

    /// 原地解密一个 16 字节分组。
    pub(crate) fn decrypt_block(&self, block: &mut [u8; 16]) {
        let table = inv_sbox();
        add_round_key(block, &self.round_keys[self.rounds * 16..self.rounds * 16 + 16]);
        for round in (1..self.rounds).rev() {
            inv_shift_rows(block);
            sub_bytes(block, table);
            add_round_key(block, &self.round_keys[round * 16..round * 16 + 16]);
            inv_mix_columns(block);
        }
        inv_shift_rows(block);
        sub_bytes(block, table);
        add_round_key(block, &self.round_keys[..16]);
    }
}

fn add_round_key(state: &mut [u8; 16], key: &[u8]) {
    for (slot, byte) in state.iter_mut().zip(key.iter()) {
        *slot ^= byte;
    }
}

fn sub_bytes(state: &mut [u8; 16], table: &[u8; 256]) {
    for byte in state.iter_mut() {
        *byte = table[*byte as usize];
    }
}

fn shift_rows(state: &mut [u8; 16]) {
    // 列主序：第 r 行左移 r 位。
    let copy = *state;
    for row in 1..4 {
        for column in 0..4 {
            state[row + 4 * column] = copy[row + 4 * ((column + row) % 4)];
        }
    }
}

fn inv_shift_rows(state: &mut [u8; 16]) {
    // 列主序：第 r 行右移 r 位，等价于左移 4 - r 位。
    let copy = *state;
    for row in 1..4 {
        for column in 0..4 {
            state[row + 4 * column] = copy[row + 4 * ((column + 4 - row) % 4)];
        }
    }
}

fn mix_columns(state: &mut [u8; 16]) {
    for column in 0..4 {
        let index = column * 4;
        let a0 = state[index];
        let a1 = state[index + 1];
        let a2 = state[index + 2];
        let a3 = state[index + 3];
        let all = a0 ^ a1 ^ a2 ^ a3;
        state[index] = a0 ^ all ^ xtime(a0 ^ a1);
        state[index + 1] = a1 ^ all ^ xtime(a1 ^ a2);
        state[index + 2] = a2 ^ all ^ xtime(a2 ^ a3);
        state[index + 3] = a3 ^ all ^ xtime(a3 ^ a0);
    }
}

fn inv_mix_columns(state: &mut [u8; 16]) {
    for column in 0..4 {
        let index = column * 4;
        let a0 = state[index];
        let a1 = state[index + 1];
        let a2 = state[index + 2];
        let a3 = state[index + 3];
        state[index] = gf_mul(a0, 14) ^ gf_mul(a1, 11) ^ gf_mul(a2, 13) ^ gf_mul(a3, 9);
        state[index + 1] = gf_mul(a0, 9) ^ gf_mul(a1, 14) ^ gf_mul(a2, 11) ^ gf_mul(a3, 13);
        state[index + 2] = gf_mul(a0, 13) ^ gf_mul(a1, 9) ^ gf_mul(a2, 14) ^ gf_mul(a3, 11);
        state[index + 3] = gf_mul(a0, 11) ^ gf_mul(a1, 13) ^ gf_mul(a2, 9) ^ gf_mul(a3, 14);
    }
}

fn xtime(value: u8) -> u8 {
    if value & 0x80 != 0 { (value << 1) ^ 0x1b } else { value << 1 }
}

fn expand_key(key: &[u8], rounds: usize) -> Vec<u8> {
    let nk = key.len() / 4;
    let words = 4 * (rounds + 1);
    let mut expanded = vec![0u8; words * 4];
    expanded[..key.len()].copy_from_slice(key);
    let table = sbox();
    let mut rcon = 1u8;
    for word in nk..words {
        let mut temp = [
            expanded[(word - 1) * 4],
            expanded[(word - 1) * 4 + 1],
            expanded[(word - 1) * 4 + 2],
            expanded[(word - 1) * 4 + 3],
        ];
        if word % nk == 0 {
            temp.rotate_left(1);
            for byte in temp.iter_mut() {
                *byte = table[*byte as usize];
            }
            temp[0] ^= rcon;
            rcon = xtime(rcon);
        }
        else if nk > 6 && word % nk == 4 {
            for byte in temp.iter_mut() {
                *byte = table[*byte as usize];
            }
        }
        for index in 0..4 {
            expanded[word * 4 + index] = expanded[(word - nk) * 4 + index] ^ temp[index];
        }
    }
    expanded
}

/// 由 GF(2^8) 乘法逆元与仿射变换现算 S-box，避免手抄 256 项常量表。
pub(crate) fn sbox() -> &'static [u8; 256] {
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u8; 256];
        for (value, slot) in table.iter_mut().enumerate() {
            let inverse = gf_inverse(value as u8);
            *slot = inverse
                ^ inverse.rotate_left(1)
                ^ inverse.rotate_left(2)
                ^ inverse.rotate_left(3)
                ^ inverse.rotate_left(4)
                ^ 0x63;
        }
        table
    })
}

/// 由正向 S-box 反查得到逆 S-box。
fn inv_sbox() -> &'static [u8; 256] {
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u8; 256];
        for (value, slot) in sbox().iter().enumerate() {
            table[*slot as usize] = value as u8;
        }
        table
    })
}

/// GF(2^8) 乘法，模多项式 0x11b。
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            product ^= a;
        }
        let carry = a & 0x80;
        a <<= 1;
        if carry != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    product
}

/// GF(2^8) 乘法逆元，`0` 映射为 `0`（AES S-box 约定）。
fn gf_inverse(value: u8) -> u8 {
    if value == 0 {
        return 0;
    }
    let mut result = 1u8;
    let mut base = value;
    let mut exponent = 254u32;
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = gf_mul(result, base);
        }
        base = gf_mul(base, base);
        exponent >>= 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{AesKey, inv_sbox, sbox};

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex")).collect()
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sbox_matches_fips_197() {
        let table = sbox();
        assert_eq!(table[0x00], 0x63);
        assert_eq!(table[0x01], 0x7c);
        assert_eq!(table[0x53], 0xed);
        assert_eq!(table[0xff], 0x16);
    }

    #[test]
    fn inverse_sbox_round_trips() {
        let inverse = inv_sbox();
        for (value, slot) in sbox().iter().enumerate() {
            assert_eq!(inverse[*slot as usize] as usize, value);
        }
    }

    #[test]
    fn aes_128_block_matches_fips_197() {
        let key = hex("000102030405060708090a0b0c0d0e0f");
        let cipher = AesKey::new(&key).expect("key");
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().expect("block");
        cipher.encrypt_block(&mut block);
        assert_eq!(to_hex(&block), "69c4e0d86a7b0430d8cdb78070b4c55a");
        cipher.decrypt_block(&mut block);
        assert_eq!(to_hex(&block), "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn aes_256_block_matches_fips_197() {
        let key = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
        let cipher = AesKey::new(&key).expect("key");
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().expect("block");
        cipher.encrypt_block(&mut block);
        assert_eq!(to_hex(&block), "8ea2b7ca516745bfeafc49904b496089");
        cipher.decrypt_block(&mut block);
        assert_eq!(to_hex(&block), "00112233445566778899aabbccddeeff");
    }
}
