//! 自有 AES-GCM 认证加密实现。
//!
//! 遵循 FIPS 197（AES）与 NIST SP 800-38D（GCM）。seal 输出为「密文 ‖ 16 字节认证标签」，
//! open 接受同一形态。nonce 必须由调用方给出并保证在密钥生命周期内唯一，库不生成 nonce，
//! 也不提供重用防护模式。AES 只实现加密方向，CTR 与 GCM 都只需这一方向。

use crate::{CryptoError, Result};
use std::sync::OnceLock;

/// AES-GCM 的 nonce 长度（96 bit）。
const NONCE_LEN: usize = 12;
/// GCM 认证标签长度。
const TAG_LEN: usize = 16;

/// 以 AES-128-GCM 加密。`key` 必须为 16 字节，`nonce` 必须为 12 字节。
pub fn aes_128_gcm_seal(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    gcm_seal(key, nonce, aad, plaintext)
}

/// 以 AES-128-GCM 解密并验证标签。失败统一返回 [`CryptoError::Authentication`]。
pub fn aes_128_gcm_open(key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    gcm_open(key, nonce, aad, ciphertext)
}

/// 以 AES-256-GCM 加密。`key` 必须为 32 字节，`nonce` 必须为 12 字节。
pub fn aes_256_gcm_seal(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    gcm_seal(key, nonce, aad, plaintext)
}

/// 以 AES-256-GCM 解密并验证标签。失败统一返回 [`CryptoError::Authentication`]。
pub fn aes_256_gcm_open(key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    gcm_open(key, nonce, aad, ciphertext)
}

fn gcm_seal(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let schedule = AesSchedule::new(key, nonce)?;
    // seal 时 GHASH 覆盖刚产生的密文。
    let (mut ciphertext, tag) = schedule.gcm(aad, plaintext, false);
    ciphertext.extend_from_slice(&tag);
    Ok(ciphertext)
}

fn gcm_open(key: &[u8], nonce: &[u8], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < TAG_LEN {
        return Err(CryptoError::Authentication);
    }
    let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_LEN);
    let schedule = AesSchedule::new(key, nonce)?;
    // open 时 GHASH 必须覆盖传入的密文，而不是 CTR 解出的明文。
    let (plaintext, expected) = schedule.gcm(aad, ciphertext, true);
    if !constant_time_eq(&expected, tag) {
        return Err(CryptoError::Authentication);
    }
    Ok(plaintext)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 展开后的 AES 密钥编排，附带 GCM 的初始计数器。
struct AesSchedule {
    round_keys: Vec<u8>,
    rounds: usize,
    /// J0：96 bit nonce 后接 0x00000001。
    j0: [u8; 16],
}

impl AesSchedule {
    fn new(key: &[u8], nonce: &[u8]) -> Result<Self> {
        if nonce.len() != NONCE_LEN {
            return Err(CryptoError::InvalidInput);
        }
        let rounds = match key.len() {
            16 => 10,
            32 => 14,
            _ => return Err(CryptoError::InvalidInput),
        };
        let mut j0 = [0u8; 16];
        j0[..NONCE_LEN].copy_from_slice(nonce);
        j0[15] = 1;
        Ok(Self { round_keys: expand_key(key, rounds), rounds, j0 })
    }

    fn encrypt_block(&self, block: &mut [u8; 16]) {
        let sbox = sbox();
        add_round_key(block, &self.round_keys[..16]);
        for round in 1..self.rounds {
            for byte in block.iter_mut() {
                *byte = sbox[*byte as usize];
            }
            shift_rows(block);
            mix_columns(block);
            add_round_key(block, &self.round_keys[round * 16..round * 16 + 16]);
        }
        for byte in block.iter_mut() {
            *byte = sbox[*byte as usize];
        }
        shift_rows(block);
        add_round_key(block, &self.round_keys[self.rounds * 16..self.rounds * 16 + 16]);
    }

    /// GCM 的 CTR 变换与 GHASH 认证。
    ///
    /// `authenticate_input` 为真时 GHASH 覆盖 `input`（解密方向，输入就是密文），否则覆盖
    /// CTR 输出（加密方向，输出就是密文）。返回（变换结果，标签）。
    fn gcm(&self, aad: &[u8], input: &[u8], authenticate_input: bool) -> (Vec<u8>, [u8; 16]) {
        let mut h = [0u8; 16];
        self.encrypt_block(&mut h);

        let mut counter = self.j0;
        let mut output = Vec::with_capacity(input.len());
        for chunk in input.chunks(16) {
            increment_counter(&mut counter);
            let mut keystream = counter;
            self.encrypt_block(&mut keystream);
            for (index, byte) in chunk.iter().enumerate() {
                output.push(byte ^ keystream[index]);
            }
        }

        let authenticated: &[u8] = if authenticate_input { input } else { &output };
        let mut state = [0u8; 16];
        ghash_update(&mut state, &h, aad);
        ghash_update(&mut state, &h, authenticated);
        let mut lengths = [0u8; 16];
        lengths[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
        lengths[8..].copy_from_slice(&((authenticated.len() as u64) * 8).to_be_bytes());
        xor_into(&mut state, &lengths);
        multiply_gf128(&mut state, &h);

        let mut tag = self.j0;
        self.encrypt_block(&mut tag);
        xor_into(&mut tag, &state);
        (output, tag)
    }
}

fn add_round_key(state: &mut [u8; 16], key: &[u8]) {
    for (slot, byte) in state.iter_mut().zip(key.iter()) {
        *slot ^= byte;
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

fn xtime(value: u8) -> u8 {
    if value & 0x80 != 0 { (value << 1) ^ 0x1b } else { value << 1 }
}

fn expand_key(key: &[u8], rounds: usize) -> Vec<u8> {
    let nk = key.len() / 4;
    let words = 4 * (rounds + 1);
    let mut expanded = vec![0u8; words * 4];
    expanded[..key.len()].copy_from_slice(key);
    let sbox = sbox();
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
                *byte = sbox[*byte as usize];
            }
            temp[0] ^= rcon;
            rcon = xtime(rcon);
        }
        else if nk > 6 && word % nk == 4 {
            for byte in temp.iter_mut() {
                *byte = sbox[*byte as usize];
            }
        }
        for index in 0..4 {
            expanded[word * 4 + index] = expanded[(word - nk) * 4 + index] ^ temp[index];
        }
    }
    expanded
}

/// 由 GF(2^8) 乘法逆元与仿射变换现算 S-box，避免手抄 256 项常量表。
fn sbox() -> &'static [u8; 256] {
    static TABLE: OnceLock<[u8; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u8; 256];
        for (value, slot) in table.iter_mut().enumerate() {
            let inverse = gf8_inverse(value as u8);
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

/// GF(2^8) 乘法，模多项式 0x11b。
fn gf8_multiply(mut a: u8, mut b: u8) -> u8 {
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
fn gf8_inverse(value: u8) -> u8 {
    if value == 0 {
        return 0;
    }
    let mut result = 1u8;
    let mut base = value;
    let mut exponent = 254u32;
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = gf8_multiply(result, base);
        }
        base = gf8_multiply(base, base);
        exponent >>= 1;
    }
    result
}

fn increment_counter(counter: &mut [u8; 16]) {
    let value = u32::from_be_bytes([counter[12], counter[13], counter[14], counter[15]]).wrapping_add(1);
    counter[12..].copy_from_slice(&value.to_be_bytes());
}

fn xor_into(state: &mut [u8; 16], other: &[u8]) {
    for (slot, byte) in state.iter_mut().zip(other.iter()) {
        *slot ^= byte;
    }
}

/// 把 `data` 补齐到 16 字节分组后逐块吸收进 GHASH 状态。
fn ghash_update(state: &mut [u8; 16], h: &[u8; 16], data: &[u8]) {
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        xor_into(state, &block);
        multiply_gf128(state, h);
    }
}

/// GF(2^128) 乘法，按 GCM 的位序（最高位在前，多项式 R = 0xe1）。
fn multiply_gf128(state: &mut [u8; 16], h: &[u8; 16]) {
    let mut z = [0u8; 16];
    let mut v = *h;
    for index in 0..128 {
        let bit = (state[index / 8] >> (7 - index % 8)) & 1;
        if bit == 1 {
            xor_into(&mut z, &v);
        }
        let carry = v[15] & 1;
        for position in (1..16).rev() {
            v[position] = (v[position] >> 1) | ((v[position - 1] & 1) << 7);
        }
        v[0] >>= 1;
        if carry == 1 {
            v[0] ^= 0xe1;
        }
    }
    *state = z;
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn aes_128_block_matches_fips_197() {
        let key = hex("000102030405060708090a0b0c0d0e0f");
        let schedule = AesSchedule::new(&key, &[0u8; NONCE_LEN]).expect("schedule");
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().expect("block");
        schedule.encrypt_block(&mut block);
        assert_eq!(to_hex(&block), "69c4e0d86a7b0430d8cdb78070b4c55a");
    }

    #[test]
    fn aes_256_block_matches_fips_197() {
        let key = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
        let schedule = AesSchedule::new(&key, &[0u8; NONCE_LEN]).expect("schedule");
        let mut block: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().expect("block");
        schedule.encrypt_block(&mut block);
        assert_eq!(to_hex(&block), "8ea2b7ca516745bfeafc49904b496089");
    }

    #[test]
    fn gcm_empty_plaintext_matches_nist_case_1() {
        let sealed = aes_128_gcm_seal(&[0u8; 16], &[0u8; NONCE_LEN], b"", b"").expect("seal");
        assert_eq!(to_hex(&sealed), "58e2fccefa7e3061367f1d57a4e7455a");
    }

    #[test]
    fn gcm_single_block_matches_nist_case_2() {
        let sealed = aes_128_gcm_seal(&[0u8; 16], &[0u8; NONCE_LEN], b"", &[0u8; 16]).expect("seal");
        assert_eq!(to_hex(&sealed), "0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf");
    }

    #[test]
    fn gcm_aes256_matches_nist_case_13_and_14() {
        let empty = aes_256_gcm_seal(&[0u8; 32], &[0u8; NONCE_LEN], b"", b"").expect("seal");
        assert_eq!(to_hex(&empty), "530f8afbc74536b9a963b4f1c4cb738b");
        let block = aes_256_gcm_seal(&[0u8; 32], &[0u8; NONCE_LEN], b"", &[0u8; 16]).expect("seal");
        assert_eq!(to_hex(&block), "cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919");
    }

    #[test]
    fn roundtrip_128_and_256() {
        let key128 = [0x42u8; 16];
        let key256 = [0x42u8; 32];
        let nonce = [0x07u8; NONCE_LEN];
        let plaintext = b"boost crypto aead";
        let sealed = aes_128_gcm_seal(&key128, &nonce, b"aad", plaintext).expect("seal");
        assert_eq!(aes_128_gcm_open(&key128, &nonce, b"aad", &sealed).expect("open"), plaintext);
        let sealed = aes_256_gcm_seal(&key256, &nonce, b"", plaintext).expect("seal");
        assert_eq!(aes_256_gcm_open(&key256, &nonce, b"", &sealed).expect("open"), plaintext);
    }

    #[test]
    fn tampered_ciphertext_rejected() {
        let key = [0x42u8; 16];
        let nonce = [0x07u8; NONCE_LEN];
        let mut sealed = aes_128_gcm_seal(&key, &nonce, b"aad", b"data").expect("seal");
        sealed[0] ^= 0x01;
        assert_eq!(aes_128_gcm_open(&key, &nonce, b"aad", &sealed), Err(CryptoError::Authentication));
    }

    #[test]
    fn tampered_tag_and_aad_rejected() {
        let key = [0x42u8; 16];
        let nonce = [0x07u8; NONCE_LEN];
        let sealed = aes_128_gcm_seal(&key, &nonce, b"aad", b"data").expect("seal");
        let last = sealed.len() - 1;
        let mut bad_tag = sealed.clone();
        bad_tag[last] ^= 0x80;
        assert_eq!(aes_128_gcm_open(&key, &nonce, b"aad", &bad_tag), Err(CryptoError::Authentication));
        assert_eq!(aes_128_gcm_open(&key, &nonce, b"other", &sealed), Err(CryptoError::Authentication));
    }

    #[test]
    fn wrong_nonce_length_rejected() {
        assert_eq!(aes_128_gcm_seal(&[0u8; 16], &[0u8; 8], b"", b""), Err(CryptoError::InvalidInput));
    }

    #[test]
    fn multi_block_and_long_aad_roundtrip() {
        let key = [0x11u8; 16];
        let nonce = [0x22u8; NONCE_LEN];
        let aad = vec![0x33u8; 37];
        let plaintext = vec![0x44u8; 1000];
        let sealed = aes_128_gcm_seal(&key, &nonce, &aad, &plaintext).expect("seal");
        assert_eq!(sealed.len(), plaintext.len() + TAG_LEN);
        assert_eq!(aes_128_gcm_open(&key, &nonce, &aad, &sealed).expect("open"), plaintext);
    }
}
