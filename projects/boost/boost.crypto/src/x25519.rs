//! 自有 X25519 密钥交换（RFC 7748）。
//!
//! 标量乘法在 GF(2^255-19) 上以 5×51 位肢体实现，Montgomery ladder 常时运行。
//! 私钥是 32 字节宿主熵，以 [`SecretBytes`](crate::secret::SecretBytes) 承载。本模块只做原始
//! DH 数学，不做协议层的密钥确认。

use crate::{CryptoError, Result, entropy::fill_host, secret::SecretBytes};

/// X25519 密钥长度（字节）。
const KEY_LEN: usize = 32;
/// 单肢体掩码（51 位）。
const MASK: u64 = (1u64 << 51) - 1;
/// `a24 = (486662 - 2) / 4`。
const A24: u64 = 121_665;

/// 用宿主安全熵生成 X25519 私钥。
///
/// 熵读取失败按 [`CryptoError::Entropy`] 传回，不返回可用私钥。
pub fn generate_secret_key() -> Result<SecretBytes> {
    let mut bytes = [0u8; KEY_LEN];
    fill_host(&mut bytes)?;
    Ok(SecretBytes::new(bytes.to_vec()))
}

/// 由私钥派生 32 字节公钥（X25519(k, 9)）。
pub fn public_key(secret: &SecretBytes) -> Result<[u8; KEY_LEN]> {
    let scalar = clamped(secret)?;
    let mut base = [0u8; KEY_LEN];
    base[0] = 9;
    Ok(x25519(&scalar, &base))
}

/// 计算共享秘密 `X25519(secret, peer_public)`。
///
/// 返回的 `SecretBytes` 在 drop 时清零；调用方应在交给 HKDF 之前不复制明文。
pub fn shared_secret(secret: &SecretBytes, peer_public: &[u8; KEY_LEN]) -> Result<SecretBytes> {
    let scalar = clamped(secret)?;
    Ok(SecretBytes::new(x25519(&scalar, peer_public).to_vec()))
}

fn clamped(secret: &SecretBytes) -> Result<[u8; KEY_LEN]> {
    if secret.len() != KEY_LEN {
        return Err(CryptoError::InvalidInput);
    }
    let mut scalar = [0u8; KEY_LEN];
    scalar.copy_from_slice(secret.expose());
    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    Ok(scalar)
}

/// RFC 7748 §5 的 X25519 函数，`scalar` 需已 clamp。
fn x25519(scalar: &[u8; KEY_LEN], point: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    let x1 = Fe::from_bytes(point);
    let mut x2 = Fe::ONE;
    let mut z2 = Fe::ZERO;
    let mut x3 = x1;
    let mut z3 = Fe::ONE;
    let mut swap = 0u64;

    for position in (0..255).rev() {
        let bit = ((scalar[position / 8] >> (position % 8)) & 1) as u64;
        swap ^= bit;
        conditional_swap(swap, &mut x2, &mut x3);
        conditional_swap(swap, &mut z2, &mut z3);
        swap = bit;

        let a = x2.add(&z2);
        let aa = a.square();
        let b = x2.sub(&z2);
        let bb = b.square();
        let e = aa.sub(&bb);
        let c = x3.add(&z3);
        let d = x3.sub(&z3);
        let da = d.mul(&a);
        let cb = c.mul(&b);
        x3 = da.add(&cb).square();
        z3 = x1.mul(&da.sub(&cb).square());
        x2 = aa.mul(&bb);
        z2 = e.mul(&aa.add(&e.mul_small(A24)));
    }

    conditional_swap(swap, &mut x2, &mut x3);
    conditional_swap(swap, &mut z2, &mut z3);

    x2.mul(&z2.invert()).to_bytes()
}

fn conditional_swap(swap: u64, left: &mut Fe, right: &mut Fe) {
    let mask = 0u64.wrapping_sub(swap);
    for index in 0..5 {
        let difference = mask & (left.0[index] ^ right.0[index]);
        left.0[index] ^= difference;
        right.0[index] ^= difference;
    }
}

/// GF(2^255-19) 元素，5×51 位肢体，小端。
#[derive(Clone, Copy)]
struct Fe([u64; 5]);

impl Fe {
    const ZERO: Self = Self([0, 0, 0, 0, 0]);
    const ONE: Self = Self([1, 0, 0, 0, 0]);

    fn from_bytes(bytes: &[u8; KEY_LEN]) -> Self {
        let load = |offset: usize| -> u64 {
            let mut chunk = [0u8; 8];
            chunk.copy_from_slice(&bytes[offset..offset + 8]);
            u64::from_le_bytes(chunk)
        };
        Self([
            load(0) & MASK,
            (load(6) >> 3) & MASK,
            (load(12) >> 6) & MASK,
            (load(19) >> 1) & MASK,
            (load(24) >> 12) & MASK,
        ])
    }

    fn to_bytes(self) -> [u8; KEY_LEN] {
        let limbs = self.canonical();
        let mut out = [0u8; KEY_LEN];
        let mut accumulator = 0u128;
        let mut bits = 0u32;
        let mut index = 0usize;
        for limb in limbs {
            accumulator |= (limb as u128) << bits;
            bits += 51;
            while bits >= 8 {
                if index < KEY_LEN {
                    out[index] = accumulator as u8;
                    index += 1;
                }
                accumulator >>= 8;
                bits -= 8;
            }
        }
        if index < KEY_LEN {
            out[index] = accumulator as u8;
        }
        out
    }

    /// 完全约简到 `[0, p)`。
    fn canonical(self) -> [u64; 5] {
        let mut limbs = self.carry(|value| value);
        // 再加 19，若溢出 p 则说明原值 >= p，需要减回去。
        let mut plus = limbs;
        let mut carry = 19u128;
        for slot in plus.iter_mut() {
            let value = *slot as u128 + carry;
            *slot = (value as u64) & MASK;
            carry = value >> 51;
        }
        if carry != 0 {
            // 发生 2^255 溢出，低位即为已减 p 的结果。
            limbs = plus;
        }
        limbs
    }

    /// 逐肢体进位并把最高进位折回最低肢体（乘以 19）。
    fn carry(&self, transform: impl Fn(u64) -> u64) -> [u64; 5] {
        let mut out = [0u64; 5];
        let mut carry = 0u128;
        for index in 0..5 {
            let value = transform(self.0[index]) as u128 + carry;
            out[index] = (value as u64) & MASK;
            carry = value >> 51;
        }
        let folded = (carry as u64).wrapping_mul(19);
        let mut final_carry = out[0] as u128 + folded as u128;
        out[0] = (final_carry as u64) & MASK;
        final_carry >>= 51;
        out[1] += final_carry as u64;
        out
    }

    fn add(self, other: &Self) -> Self {
        let mut out = [0u64; 5];
        for index in 0..5 {
            out[index] = self.0[index] + other.0[index];
        }
        Self(out)
    }

    fn sub(self, other: &Self) -> Self {
        // 加 4p 保证不借位；超出 51 位的部分由后续乘法进位处理。
        const P: [u64; 5] = [
            0x7ffffffffffed,
            0x7ffffffffffff,
            0x7ffffffffffff,
            0x7ffffffffffff,
            0x7ffffffffffff,
        ];
        let mut out = [0u64; 5];
        for index in 0..5 {
            out[index] = self.0[index].wrapping_add(4 * P[index]).wrapping_sub(other.0[index]);
        }
        Self(out)
    }

    fn mul(self, other: &Self) -> Self {
        let mut products = [0u128; 5];
        for (left, left_limb) in self.0.iter().enumerate() {
            for (right, right_limb) in other.0.iter().enumerate() {
                let product = (*left_limb as u128) * (*right_limb as u128);
                let slot = left + right;
                if slot < 5 {
                    products[slot] += product;
                }
                else {
                    products[slot - 5] += product * 19;
                }
            }
        }
        Self::from_wide(products)
    }

    fn mul_small(self, factor: u64) -> Self {
        let mut products = [0u128; 5];
        for index in 0..5 {
            products[index] = (self.0[index] as u128) * (factor as u128);
        }
        Self::from_wide(products)
    }

    fn square(self) -> Self {
        self.mul(&self)
    }

    fn from_wide(products: [u128; 5]) -> Self {
        let mut out = [0u64; 5];
        let mut carry = 0u128;
        for index in 0..5 {
            let value = products[index] + carry;
            out[index] = (value as u64) & MASK;
            carry = value >> 51;
        }
        let mut value = out[0] as u128 + carry * 19;
        out[0] = (value as u64) & MASK;
        value >>= 51;
        out[1] += value as u64;
        Self(out)
    }

    /// 乘法逆元 `x^(p-2)`（Fermat）。
    fn invert(self) -> Self {
        // p - 2 = 2^255 - 21，小端字节：0xeb 后接 30 个 0xff，最后 0x7f。
        let mut exponent = [0xffu8; KEY_LEN];
        exponent[0] = 0xeb;
        exponent[31] = 0x7f;

        let mut result = Self::ONE;
        let mut base = self;
        for position in 0..255 {
            if (exponent[position / 8] >> (position % 8)) & 1 == 1 {
                result = result.mul(&base);
            }
            base = base.square();
        }
        result
    }
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

    /// RFC 7748 §5 的 `decodeScalar25519`：测试向量输入需先 clamp。
    fn clamp(mut scalar: [u8; 32]) -> [u8; 32] {
        scalar[0] &= 248;
        scalar[31] &= 127;
        scalar[31] |= 64;
        scalar
    }

    #[test]
    fn rfc7748_vector_one() {
        let scalar = clamp(hex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4").try_into().expect("scalar"));
        let point: [u8; 32] = hex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c").try_into().expect("point");
        assert_eq!(to_hex(&x25519(&scalar, &point)), "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552");
    }

    #[test]
    fn rfc7748_vector_two() {
        let scalar = clamp(hex("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d").try_into().expect("scalar"));
        let point: [u8; 32] = hex("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493").try_into().expect("point");
        assert_eq!(to_hex(&x25519(&scalar, &point)), "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957");
    }

    #[test]
    fn rfc7748_base_point_and_shared_secret() {
        let alice_secret = SecretBytes::new(hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a"));
        let bob_secret = SecretBytes::new(hex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb"));

        let alice_public = public_key(&alice_secret).expect("alice public");
        let bob_public = public_key(&bob_secret).expect("bob public");
        assert_eq!(to_hex(&alice_public), "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        assert_eq!(to_hex(&bob_public), "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");

        let alice_side = shared_secret(&alice_secret, &bob_public).expect("alice dh");
        let bob_side = shared_secret(&bob_secret, &alice_public).expect("bob dh");
        assert_eq!(to_hex(alice_side.expose()), "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        assert_eq!(alice_side.expose(), bob_side.expose());
    }

    #[test]
    fn field_arithmetic_roundtrips_bytes() {
        let bytes: [u8; 32] = hex("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20").try_into().expect("bytes");
        let element = Fe::from_bytes(&bytes);
        assert_eq!(element.to_bytes(), bytes);
    }

    #[test]
    fn inversion_is_multiplicative_inverse() {
        let value = Fe::from_bytes(&{
            let mut raw = [0u8; 32];
            raw[0] = 7;
            raw
        });
        let product = value.mul(&value.invert());
        let expected = Fe::ONE.to_bytes();
        assert_eq!(product.to_bytes(), expected);
    }

    #[test]
    fn wrong_secret_length_rejected() {
        let short = SecretBytes::new(vec![0u8; 31]);
        assert_eq!(public_key(&short), Err(CryptoError::InvalidInput));
    }
}
