//! 自有 Ed25519 验签（RFC 8032）。
//!
//! 复用 [`crate::x25519`] 的 GF(2^255-19) 域运算，在本模块补齐 Edwards 曲线点运算与
//! 压缩点编解码。只实现验签：验签的输入全部来自对端，允许变时运算；签名与密钥生成不在
//! 本模块职责内。标量不做模 L 约简，直接用 512 位哈希的原始比特做倍点，因为基点与公钥的
//! 阶都能整除 L，结果与约简后一致。

use crate::{CryptoError, Result, hash::sha512, x25519::Fe};

/// 群阶 `L = 2^252 + 27742317777372353535851937790883648493`，小端。
const ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

/// `(p - 5) / 8 = 2^252 - 3`，开方用的指数，小端。
const EXP_P58: [u8; 32] = [
    0xfd, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x0f,
];

/// `(p - 1) / 4 = 2^253 - 5`，求 `sqrt(-1)` 的指数，小端。
const EXP_M1_4: [u8; 32] = [
    0xfb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x1f,
];

/// 验证一个 Ed25519 签名。
///
/// `public_key` 是 32 字节压缩公钥，`signature` 是 `R || S` 共 64 字节。公钥无法解压、
/// `S` 不在 `[0, L)` 内、或等式 `[S]B = R + [H(R||A||M)]A` 不成立时返回
/// [`CryptoError::Authentication`]；长度不对返回 [`CryptoError::InvalidInput`]。
pub fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<()> {
    let public_key: [u8; 32] = public_key.try_into().map_err(|_| CryptoError::InvalidInput)?;
    let signature: &[u8; 64] = signature.try_into().map_err(|_| CryptoError::InvalidInput)?;
    let mut r = [0u8; 32];
    r.copy_from_slice(&signature[..32]);
    let mut s = [0u8; 32];
    s.copy_from_slice(&signature[32..]);
    if !scalar_is_canonical(&s) {
        return Err(CryptoError::Authentication);
    }

    let public = Point::decompress(&public_key).ok_or(CryptoError::Authentication)?;
    let r_point = Point::decompress(&r).ok_or(CryptoError::Authentication)?;

    let mut data = Vec::with_capacity(64 + message.len());
    data.extend_from_slice(&r);
    data.extend_from_slice(&public_key);
    data.extend_from_slice(message);
    let hash = sha512(&data);

    let left = Point::base().scalar_mul(&s);
    let right = r_point.add(&public.scalar_mul(&hash));
    if left.compress() == right.compress() { Ok(()) } else { Err(CryptoError::Authentication) }
}

/// `s < L`，按小端逐字节从高位比较。
fn scalar_is_canonical(scalar: &[u8; 32]) -> bool {
    for index in (0..32).rev() {
        if scalar[index] < ORDER[index] {
            return true;
        }
        if scalar[index] > ORDER[index] {
            return false;
        }
    }
    false
}

/// 曲线常数 `d = -121665 / 121666 mod p`。
fn curve_d() -> Fe {
    Fe::from_small(121_665).neg().mul(&Fe::from_small(121_666).invert())
}

/// `2d`，逐次加法都要用，缓存避免反复求逆。
fn two_d() -> Fe {
    static TWO_D: std::sync::OnceLock<Fe> = std::sync::OnceLock::new();
    *TWO_D.get_or_init(|| curve_d().mul(&Fe::from_small(2)))
}

/// `sqrt(-1) = 2^((p-1)/4) mod p`。
fn sqrt_m1() -> Fe {
    static SQRT_M1: std::sync::OnceLock<Fe> = std::sync::OnceLock::new();
    *SQRT_M1.get_or_init(|| Fe::from_small(2).pow_bytes(&EXP_M1_4))
}

/// 扩展扭曲 Edwards 坐标点，`t = x * y / z`。
#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl Point {
    fn identity() -> Self {
        Self { x: Fe::ZERO, y: Fe::ONE, z: Fe::ONE, t: Fe::ZERO }
    }

    /// 基点 `B`，压缩编码为 `y = 4/5` 且符号位为 0。
    fn base() -> Self {
        let mut compressed = Fe::from_small(4).mul(&Fe::from_small(5).invert()).to_bytes();
        compressed[31] &= 0x7f;
        Self::decompress(&compressed).expect("the base point is on the curve")
    }

    /// 完全加法公式（`a = -1`），对恒等元与倍点同样成立。
    fn add(&self, other: &Point) -> Point {
        let a = self.y.sub(&self.x).mul(&other.y.sub(&other.x));
        let b = self.y.add(&self.x).mul(&other.y.add(&other.x));
        let c = self.t.mul(&two_d()).mul(&other.t);
        let d = self.z.mul(&other.z).mul(&Fe::from_small(2));
        let e = b.sub(&a);
        let f = d.sub(&c);
        let g = d.add(&c);
        let h = b.add(&a);
        Self { x: e.mul(&f), y: g.mul(&h), t: e.mul(&h), z: f.mul(&g) }
    }

    /// 变时倍点加法，标量来自公开哈希或签名。
    fn scalar_mul(&self, scalar: &[u8]) -> Point {
        let mut result = Self::identity();
        for byte in scalar.iter().rev() {
            for bit in (0..8).rev() {
                result = result.add(&result);
                if (byte >> bit) & 1 == 1 {
                    result = result.add(self);
                }
            }
        }
        result
    }

    fn compress(&self) -> [u8; 32] {
        let inverse = self.z.invert();
        let x = self.x.mul(&inverse);
        let y = self.y.mul(&inverse);
        let mut out = y.to_bytes();
        out[31] |= u8::from(x.is_odd()) << 7;
        out
    }

    /// 由 32 字节压缩点恢复坐标；非规范 `y`、无平方根或 `x = 0` 却置符号位时返回 `None`。
    fn decompress(bytes: &[u8; 32]) -> Option<Self> {
        let sign = bytes[31] >> 7;
        let mut y_bytes = *bytes;
        y_bytes[31] &= 0x7f;
        let y = Fe::from_bytes(&y_bytes);
        if y.to_bytes() != y_bytes {
            return None;
        }
        let y2 = y.square();
        let u = y2.sub(&Fe::ONE);
        let v = curve_d().mul(&y2).add(&Fe::ONE);
        let x = sqrt_ratio(&u, &v)?;
        let x = if x.is_odd() == (sign == 1) { x } else { x.neg() };
        if x.to_bytes() == Fe::ZERO.to_bytes() && sign == 1 {
            return None;
        }
        Some(Self { x, y, z: Fe::ONE, t: x.mul(&y) })
    }
}

/// 求 `sqrt(u / v)`，按 RFC 8032 §5.1.3 用 `sqrt(-1)` 校正。
fn sqrt_ratio(u: &Fe, v: &Fe) -> Option<Fe> {
    let v3 = v.square().mul(v);
    let v7 = v3.square().mul(v);
    let mut root = u.mul(&v3).mul(&u.mul(&v7).pow_bytes(&EXP_P58));
    if v.mul(&root.square()).to_bytes() == u.to_bytes() {
        return Some(root);
    }
    let sqrt_m1 = sqrt_m1();
    root = root.mul(&sqrt_m1);
    if v.mul(&root.square()).to_bytes() == u.to_bytes() { Some(root) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex")).collect()
    }

    /// RFC 8032 §7.1 TEST 1：空消息。
    #[test]
    fn rfc8032_empty_message() {
        let public = hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let signature = hex(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
             5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );
        verify(&public, b"", &signature).expect("valid signature");
        verify(&public, b"x", &signature).expect_err("message must be bound");
    }

    /// RFC 8032 §7.1 TEST 2：单字节消息。
    #[test]
    fn rfc8032_one_byte_message() {
        let public = hex("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
        let signature = hex(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
             085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        );
        verify(&public, &[0x72], &signature).expect("valid signature");
    }

    /// RFC 8032 §7.1 TEST 3：两字节消息。
    #[test]
    fn rfc8032_two_byte_message() {
        let public = hex("fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025");
        let signature = hex(
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
             18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
        );
        verify(&public, &[0xaf, 0x82], &signature).expect("valid signature");
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let public = hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let mut signature = hex(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
             5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );
        signature[0] ^= 0x01;
        assert_eq!(verify(&public, b"", &signature), Err(CryptoError::Authentication));
    }

    #[test]
    fn wrong_lengths_are_invalid_input() {
        assert_eq!(verify(&[0u8; 31], b"", &[0u8; 64]), Err(CryptoError::InvalidInput));
        assert_eq!(verify(&[0u8; 32], b"", &[0u8; 63]), Err(CryptoError::InvalidInput));
    }

    #[test]
    fn base_point_round_trips() {
        let compressed = Point::base().compress();
        let restored = Point::decompress(&compressed).expect("on the curve");
        assert_eq!(restored.compress(), compressed);
    }

    #[test]
    fn non_canonical_scalar_is_rejected() {
        assert!(!scalar_is_canonical(&ORDER));
        let mut one = [0u8; 32];
        one[0] = 1;
        assert!(scalar_is_canonical(&one));
    }
}
