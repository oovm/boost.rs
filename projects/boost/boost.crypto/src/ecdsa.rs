//! ECDSA 签名与验签（NIST P-256 与 P-384），自有实现。
//!
//! 签名输出同时提供 DER 与 fixed-width（r ‖ s）两种编码并可互转。签名由 RFC 6979 确定性 nonce
//! 生成，不需要运行期随机数；私钥生成走宿主熵。素域与标量域算术以 `[u64]` 肢体自实现，
//! 不依赖外部椭圆曲线或大整数 crate。本模块只做原语执行，不承载协议层的密钥编码语义。

use crate::{CryptoError, Result, entropy::fill_host};

fn hmac_sha256_bytes(key: &[u8], data: &[u8]) -> [u8; 32] {
    crate::mac::hmac_sha256(key, data).expect("hmac-sha256 never fails")
}

fn hmac_sha384_bytes(key: &[u8], data: &[u8]) -> [u8; 48] {
    crate::mac::hmac_sha384(key, data).expect("hmac-sha384 never fails")
}

/// 以 `N` 个 64 位小端肢体表示的非负整数。
#[derive(Clone, Copy, PartialEq, Eq)]
struct UInt<const N: usize>([u64; N]);

impl<const N: usize> UInt<N> {
    const ZERO: Self = Self([0; N]);

    fn one() -> Self {
        let mut out = [0u64; N];
        out[0] = 1;
        Self(out)
    }

    fn from_be_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > N * 8 {
            return None;
        }
        let mut limbs = [0u64; N];
        let pad = N * 8 - bytes.len();
        for (index, byte) in bytes.iter().enumerate() {
            let absolute = pad + index;
            let limb = N - 1 - absolute / 8;
            let shift = (absolute % 8) * 8;
            limbs[limb] |= (*byte as u64) << (56 - shift);
        }
        Some(Self(limbs))
    }

    fn to_be_bytes(self) -> Vec<u8> {
        let mut out = vec![0u8; N * 8];
        for limb in 0..N {
            let value = self.0[N - 1 - limb].to_be_bytes();
            out[limb * 8..limb * 8 + 8].copy_from_slice(&value);
        }
        out
    }

    fn is_zero(self) -> bool {
        self.0.iter().all(|limb| *limb == 0)
    }

    fn bit(self, position: usize) -> u64 {
        let limb = position / 64;
        if limb >= N { 0 } else { (self.0[limb] >> (position % 64)) & 1 }
    }
}

fn ge(a: &[u64], b: &[u64]) -> bool {
    for index in (0..a.len()).rev() {
        if a[index] != b[index] {
            return a[index] > b[index];
        }
    }
    true
}

fn sub_assign(a: &mut [u64], b: &[u64]) {
    let mut borrow = 0i128;
    for index in 0..a.len() {
        let diff = a[index] as i128 - b[index] as i128 - borrow;
        if diff < 0 {
            a[index] = (diff + (1i128 << 64)) as u64;
            borrow = 1;
        }
        else {
            a[index] = diff as u64;
            borrow = 0;
        }
    }
}

/// 位运算取模，仅用于一次性预计算与哈希截断，热点路径使用 Montgomery。
fn rem_mod<const N: usize>(value: &[u64], m: &[u64; N]) -> UInt<N> {
    // 余数在左移后可达 2*m-1，需要 N+1 个肢体容纳溢出。
    let mut remainder = vec![0u64; N + 1];
    let mut m_ext = vec![0u64; N + 1];
    m_ext[..N].copy_from_slice(m);
    let total_bits = value.len() * 64;
    for bit in (0..total_bits).rev() {
        let mut carry = 0u64;
        for limb in remainder.iter_mut() {
            let new_carry = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = new_carry;
        }
        let in_bit = (value[bit / 64] >> (bit % 64)) & 1;
        remainder[0] |= in_bit;
        if ge(&remainder, &m_ext) {
            sub_assign(&mut remainder, &m_ext);
        }
    }
    let mut out = [0u64; N];
    out.copy_from_slice(&remainder[..N]);
    UInt(out)
}

fn zero_extend<const N: usize>(value: UInt<N>) -> Vec<u64> {
    let mut out = vec![0u64; 2 * N];
    out[..N].copy_from_slice(&value.0);
    out
}

fn mod_add<const N: usize>(a: UInt<N>, b: UInt<N>, m: &[u64; N]) -> UInt<N> {
    let mut carry = 0u128;
    let mut out = [0u64; N];
    for index in 0..N {
        let value = a.0[index] as u128 + b.0[index] as u128 + carry;
        out[index] = value as u64;
        carry = value >> 64;
    }
    let mut result = UInt(out);
    if carry != 0 || ge(&result.0, m) {
        sub_assign(&mut result.0, m);
    }
    result
}

fn mod_sub<const N: usize>(a: UInt<N>, b: UInt<N>, m: &[u64; N]) -> UInt<N> {
    let mut borrow = 0i128;
    let mut out = [0u64; N];
    for index in 0..N {
        let diff = a.0[index] as i128 - b.0[index] as i128 - borrow;
        if diff < 0 {
            out[index] = (diff + (1i128 << 64)) as u64;
            borrow = 1;
        }
        else {
            out[index] = diff as u64;
            borrow = 0;
        }
    }
    if borrow != 0 {
        let mut carry = 0u128;
        for index in 0..N {
            let value = out[index] as u128 + m[index] as u128 + carry;
            out[index] = value as u64;
            carry = value >> 64;
        }
    }
    UInt(out)
}

/// Montgomery 域参数：模数 `m`、`m' = -m^-1 mod 2^64` 与 `R^2 mod m`。
struct Field<const N: usize> {
    m: [u64; N],
    m_prime: u64,
    r2: UInt<N>,
}

const fn mont_prime(m0: u64) -> u64 {
    // 先求 m^-1 mod 2^64，再取负。
    let mut x = 1u64;
    let mut i = 0;
    while i < 6 {
        x = x.wrapping_mul(2u64.wrapping_sub(m0.wrapping_mul(x)));
        i += 1;
    }
    x.wrapping_neg()
}

impl<const N: usize> Field<N> {
    fn new(m: [u64; N]) -> Self {
        let m_prime = mont_prime(m[0]);
        // R^2 = 2^(2*64*N) mod m，一次性位运算计算。
        let mut wide = vec![0u64; 2 * N + 1];
        wide[2 * N] = 1;
        let r2 = rem_mod(&wide, &m);
        Self { m, m_prime, r2 }
    }
}

/// CIOS Montgomery 乘法：`a, b` 均为 Montgomery 形式，返回 `(a*b) mod m` 的 Montgomery 形式。
#[allow(unused_assignments)]
fn mont_mul<const N: usize>(a: &UInt<N>, b: &UInt<N>, field: &Field<N>) -> UInt<N> {
    let m = &field.m;
    let m_prime = field.m_prime;
    let mut t = vec![0u64; N + 2];
    for &limb in a.0.iter().take(N) {
        let mut carry = 0u128;
        for j in 0..N {
            let value = t[j] as u128 + (limb as u128) * (b.0[j] as u128) + carry;
            t[j] = value as u64;
            carry = value >> 64;
        }
        let mut k = N;
        while carry > 0 {
            let value = t[k] as u128 + carry;
            t[k] = value as u64;
            carry = value >> 64;
            k += 1;
        }
        let u = t[0].wrapping_mul(m_prime);
        let mut carry = 0u128;
        let value = t[0] as u128 + (u as u128) * (m[0] as u128);
        carry = value >> 64;
        for j in 1..N {
            let value = t[j] as u128 + (u as u128) * (m[j] as u128) + carry;
            t[j] = value as u64;
            carry = value >> 64;
        }
        let mut k = N;
        while carry > 0 {
            let value = t[k] as u128 + carry;
            t[k] = value as u64;
            carry = value >> 64;
            k += 1;
        }
        for j in 0..=N {
            t[j] = t[j + 1];
        }
        t[N + 1] = 0;
    }
    let mut result = [0u64; N];
    result.copy_from_slice(&t[..N]);
    if t[N] != 0 || ge(&result, m) {
        sub_assign(&mut result, m);
    }
    UInt(result)
}

/// 普通形式的模乘：两端转 Montgomery、相乘、再转回。
fn mod_mul<const N: usize>(a: UInt<N>, b: UInt<N>, field: &Field<N>) -> UInt<N> {
    let am = mont_mul(&a, &field.r2, field);
    let bm = mont_mul(&b, &field.r2, field);
    let cm = mont_mul(&am, &bm, field);
    mont_mul(&cm, &UInt::one(), field)
}

fn mod_pow<const N: usize>(base: UInt<N>, exp: &UInt<N>, field: &Field<N>) -> UInt<N> {
    let mut result = UInt::one();
    let mut base = base;
    for bit in 0..N * 64 {
        if exp.bit(bit) == 1 {
            result = mod_mul(result, base, field);
        }
        base = mod_mul(base, base, field);
    }
    result
}

fn mod_inv<const N: usize>(a: UInt<N>, field: &Field<N>) -> UInt<N> {
    let mut two = UInt::ZERO;
    two.0[0] = 2;
    let exponent = mod_sub(UInt(field.m), two, &field.m);
    mod_pow(a, &exponent, field)
}

/// 雅可比坐标点。
#[derive(Clone, Copy)]
struct Point<const N: usize> {
    x: UInt<N>,
    y: UInt<N>,
    z: UInt<N>,
}

impl<const N: usize> Point<N> {
    fn identity() -> Self {
        Self { x: UInt::ZERO, y: UInt::ZERO, z: UInt::ZERO }
    }

    fn is_identity(&self) -> bool {
        self.z.is_zero()
    }

    fn from_affine(x: UInt<N>, y: UInt<N>) -> Self {
        let mut z = UInt::ZERO;
        z.0[0] = 1;
        Self { x, y, z }
    }

    /// 雅可比倍点（`dbl-2001-b`，适用 `a = -3` 即 P-256 / P-384）。
    fn double(&self, field: &Field<N>) -> Self {
        if self.is_identity() {
            return *self;
        }
        let p = &field.m;
        let delta = mod_mul(self.z, self.z, field);
        let gamma = mod_mul(self.y, self.y, field);
        let beta = mod_mul(self.x, gamma, field);
        let alpha = mod_mul(mod_sub(self.x, delta, p), mod_add(self.x, delta, p), field);
        let alpha = mod_add(mod_add(alpha, alpha, p), alpha, p);
        let two_beta = mod_add(beta, beta, p);
        let four_beta = mod_add(two_beta, two_beta, p);
        let eight_beta = mod_add(four_beta, four_beta, p);
        let x3 = mod_sub(mod_mul(alpha, alpha, field), eight_beta, p);
        let yz = mod_add(self.y, self.z, p);
        let z3 = mod_sub(mod_sub(mod_mul(yz, yz, field), gamma, p), delta, p);
        let gamma2 = mod_mul(gamma, gamma, field);
        let two_gamma2 = mod_add(gamma2, gamma2, p);
        let four_gamma2 = mod_add(two_gamma2, two_gamma2, p);
        let eight_gamma2 = mod_add(four_gamma2, four_gamma2, p);
        let y3 = mod_sub(mod_mul(alpha, mod_sub(four_beta, x3, p), field), eight_gamma2, p);
        Self { x: x3, y: y3, z: z3 }
    }

    /// 雅可比加（`add-1998-cmo-2`，同时计算 U1/S1 与 U2/S2，不假设 Z1 == 1）。
    fn add(&self, other: &Self, field: &Field<N>) -> Self {
        if self.is_identity() {
            return *other;
        }
        if other.is_identity() {
            return *self;
        }
        let p = &field.m;
        let z1z1 = mod_mul(self.z, self.z, field);
        let z2z2 = mod_mul(other.z, other.z, field);
        let u1 = mod_mul(self.x, z2z2, field);
        let u2 = mod_mul(other.x, z1z1, field);
        let s1 = mod_mul(self.y, mod_mul(other.z, z2z2, field), field);
        let s2 = mod_mul(other.y, mod_mul(self.z, z1z1, field), field);
        let h = mod_sub(u2, u1, p);
        let r = mod_sub(s2, s1, p);
        if h.is_zero() {
            return if r.is_zero() { self.double(field) } else { Self::identity() };
        }
        let h2 = mod_mul(h, h, field);
        let h3 = mod_mul(h2, h, field);
        let u1h2 = mod_mul(u1, h2, field);
        let x3 = mod_sub(mod_sub(mod_mul(r, r, field), h3, p), mod_add(u1h2, u1h2, p), p);
        let y3 = mod_sub(mod_mul(r, mod_sub(u1h2, x3, p), field), mod_mul(s1, h3, field), p);
        let z3 = mod_mul(mod_mul(h, self.z, field), other.z, field);
        Self { x: x3, y: y3, z: z3 }
    }

    fn scalar_mul(&self, scalar: UInt<N>, field: &Field<N>) -> Self {
        let mut result = Self::identity();
        for bit in (0..N * 64).rev() {
            result = result.double(field);
            if scalar.bit(bit) == 1 {
                result = result.add(self, field);
            }
        }
        result
    }

    fn affine(self, field: &Field<N>) -> Option<(UInt<N>, UInt<N>)> {
        if self.is_identity() {
            return None;
        }
        let z_inv = mod_inv(self.z, field);
        let z_inv_2 = mod_mul(z_inv, z_inv, field);
        let z_inv_3 = mod_mul(z_inv_2, z_inv, field);
        Some((mod_mul(self.x, z_inv_2, field), mod_mul(self.y, z_inv_3, field)))
    }
}

struct Curve<const N: usize> {
    p: [u64; N],
    n: [u64; N],
    gx: [u64; N],
    gy: [u64; N],
    b: &'static [u8],
    byte_len: usize,
}

impl<const N: usize> Curve<N> {
    fn generator(&self) -> Point<N> {
        Point::from_affine(UInt(self.gx), UInt(self.gy))
    }
}

const P256_B: &[u8] =
    b"\x5a\xc6\x35\xd8\xaa\x3a\x93\xe7\xb3\xeb\xbd\x55\x76\x98\x86\xbc\x65\x1d\x06\xb0\xcc\x53\xb0\xf6\x3b\xce\x3c\x3e\x27\xd2\x60\x4b";
const P384_B: &[u8] = b"\xb3\x31\x2f\xa7\xe2\x3e\xe7\xe4\x98\x8e\x05\x6b\xe3\xf8\x2d\x19\x18\x1d\x9c\x6e\xfe\x81\x41\x12\x03\x14\x08\x8f\x50\x13\x87\x5a\xc6\x56\x39\x8d\x8a\x2e\xd1\x9d\x2a\x85\xc8\xed\xd3\xec\x2a\xef";

const P256: Curve<4> = Curve {
    p: [0xFFFFFFFFFFFFFFFF, 0x00000000FFFFFFFF, 0x0000000000000000, 0xFFFFFFFF00000001],
    n: [0xF3B9CAC2FC632551, 0xBCE6FAADA7179E84, 0xFFFFFFFFFFFFFFFF, 0xFFFFFFFF00000000],
    gx: [0xF4A13945D898C296, 0x77037D812DEB33A0, 0xF8BCE6E563A440F2, 0x6B17D1F2E12C4247],
    gy: [0xCBB6406837BF51F5, 0x2BCE33576B315ECE, 0x8EE7EB4A7C0F9E16, 0x4FE342E2FE1A7F9B],
    b: P256_B,
    byte_len: 32,
};

const P384: Curve<6> = Curve {
    p: [0x00000000FFFFFFFF, 0xFFFFFFFF00000000, 0xFFFFFFFFFFFFFFFE, 0xFFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFF],
    n: [0xECEC196ACCC52973, 0x581A0DB248B0A77A, 0xC7634D81F4372DDF, 0xFFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFF],
    gx: [0x3A545E3872760AB7, 0x5502F25DBF55296C, 0x59F741E082542A38, 0x6E1D3B628BA79B98, 0x8EB1C71EF320AD74, 0xAA87CA22BE8B0537],
    gy: [0x7A431D7C90EA0E5F, 0x0A60B1CE1D7E819D, 0xE9DA3113B5F0B8C0, 0xF8F41DBD289A147C, 0x5D9E98BF9292DC29, 0x3617DE4A96262C6F],
    b: P384_B,
    byte_len: 48,
};

fn bits2int<const N: usize>(bytes: &[u8], qlen_bits: usize) -> UInt<N> {
    let total_bits = bytes.len() * 8;
    if total_bits <= qlen_bits {
        return UInt::from_be_bytes(bytes).unwrap_or(UInt::ZERO);
    }
    let byte_count = qlen_bits / 8;
    let extra = qlen_bits % 8;
    let mut buf = vec![0u8; byte_count + (extra > 0) as usize];
    buf[..byte_count].copy_from_slice(&bytes[..byte_count]);
    if extra > 0 {
        buf[byte_count] = bytes[byte_count] & (0xff << (8 - extra));
    }
    UInt::from_be_bytes(&buf).unwrap_or(UInt::ZERO)
}

fn int2octets<const N: usize>(x: UInt<N>, q: &[u64; N], byte_len: usize) -> Vec<u8> {
    let reduced = rem_mod(&zero_extend(x), q);
    let bytes = reduced.to_be_bytes();
    bytes[bytes.len() - byte_len..].to_vec()
}

fn rfc6979_k<const N: usize, const HLEN: usize>(
    x: UInt<N>,
    h1: &[u8],
    q: &[u64; N],
    qlen_bits: usize,
    byte_len: usize,
    hmac: fn(&[u8], &[u8]) -> [u8; HLEN],
) -> UInt<N> {
    let x_octets = int2octets(x, q, byte_len);
    let h1_octets = int2octets(bits2int::<N>(h1, qlen_bits), q, byte_len);

    let mut v = [0x01u8; HLEN];
    let mut k = [0x00u8; HLEN];

    let step = |k: &mut [u8; HLEN], v: &[u8; HLEN], byte: u8| {
        let mut data = Vec::with_capacity(HLEN + 1 + x_octets.len() + h1_octets.len());
        data.extend_from_slice(v);
        data.push(byte);
        data.extend_from_slice(&x_octets);
        data.extend_from_slice(&h1_octets);
        *k = hmac(k, &data);
    };

    step(&mut k, &v, 0x00);
    v = hmac(&k, &v);
    step(&mut k, &v, 0x01);
    v = hmac(&k, &v);

    loop {
        let mut t = Vec::with_capacity(byte_len);
        while t.len() < byte_len {
            v = hmac(&k, &v);
            t.extend_from_slice(&v);
        }
        t.truncate(byte_len);
        let candidate = bits2int::<N>(&t, qlen_bits);
        if !candidate.is_zero() && !ge(&candidate.0, q) {
            return candidate;
        }
        let mut data = Vec::with_capacity(HLEN + 1);
        data.extend_from_slice(&v);
        data.push(0x00);
        k = hmac(&k, &data);
        v = hmac(&k, &v);
    }
}

struct Signature<const N: usize> {
    r: UInt<N>,
    s: UInt<N>,
}

fn ecdsa_sign<const N: usize, const HLEN: usize>(
    d: UInt<N>,
    message: &[u8],
    curve: &Curve<N>,
    hash: fn(&[u8]) -> [u8; HLEN],
    hmac: fn(&[u8], &[u8]) -> [u8; HLEN],
) -> Signature<N> {
    let h = hash(message);
    let z = rem_mod(&zero_extend(bits2int::<N>(&h, N * 64)), &curve.n);
    let p_field = Field::new(curve.p);
    let n_field = Field::new(curve.n);

    loop {
        let k = rfc6979_k::<N, HLEN>(d, &h, &curve.n, N * 64, curve.byte_len, hmac);
        let point = curve.generator().scalar_mul(k, &p_field);
        let (x, _y) = point.affine(&p_field).expect("kG must not be infinity");
        let r = rem_mod(&zero_extend(x), &curve.n);
        if r.is_zero() {
            continue;
        }
        let k_inv = mod_inv(k, &n_field);
        let rd = mod_mul(r, d, &n_field);
        let z_plus_rd = mod_add(z, rd, &curve.n);
        let s = mod_mul(k_inv, z_plus_rd, &n_field);
        if s.is_zero() {
            continue;
        }
        let s = normalize_s(s, &curve.n);
        return Signature { r, s };
    }
}

fn normalize_s<const N: usize>(s: UInt<N>, n: &[u64; N]) -> UInt<N> {
    // s > n/2 时取 n - s，与主流库的低 s 规范一致。
    let mut half = UInt(*n);
    let mut carry = 0u64;
    for limb in (0..N).rev() {
        let new_carry = half.0[limb] & 1;
        half.0[limb] = (half.0[limb] >> 1) | (carry << 63);
        carry = new_carry;
    }
    if ge(&s.0, &half.0) { mod_sub(UInt(*n), s, n) } else { s }
}

fn ecdsa_verify<const N: usize, const HLEN: usize>(
    public: &(UInt<N>, UInt<N>),
    message: &[u8],
    sig: &Signature<N>,
    curve: &Curve<N>,
    hash: fn(&[u8]) -> [u8; HLEN],
) -> bool {
    let n = &curve.n;
    if sig.r.is_zero() || sig.s.is_zero() {
        return false;
    }
    if ge(&sig.r.0, n) || ge(&sig.s.0, n) {
        return false;
    }
    let h = hash(message);
    let z = rem_mod(&zero_extend(bits2int::<N>(&h, N * 64)), n);
    let n_field = Field::new(curve.n);
    let p_field = Field::new(curve.p);
    let w = mod_inv(sig.s, &n_field);
    let u1 = mod_mul(z, w, &n_field);
    let u2 = mod_mul(sig.r, w, &n_field);
    let g = curve.generator().scalar_mul(u1, &p_field);
    let q = Point::from_affine(public.0, public.1).scalar_mul(u2, &p_field);
    match g.add(&q, &p_field).affine(&p_field) {
        Some((x, _y)) => rem_mod(&zero_extend(x), n) == sig.r,
        None => false,
    }
}

fn der_encode_integer(value: &[u8]) -> Vec<u8> {
    let mut trimmed = value;
    while trimmed.len() > 1 && trimmed[0] == 0 {
        trimmed = &trimmed[1..];
    }
    let needs_pad = trimmed[0] & 0x80 != 0;
    let mut out = Vec::with_capacity(2 + trimmed.len() + 1);
    out.push(0x02);
    out.push(trimmed.len() as u8 + needs_pad as u8);
    if needs_pad {
        out.push(0x00);
    }
    out.extend_from_slice(trimmed);
    out
}

fn der_decode_signature(der: &[u8], byte_len: usize) -> Result<(Vec<u8>, Vec<u8>)> {
    if der.len() < 2 || der[0] != 0x30 {
        return Err(CryptoError::InvalidInput);
    }
    let (seq_len, len_bytes) = der_length(&der[1..])?;
    let body_start = 1 + len_bytes + 1;
    let seq_end = body_start + seq_len;
    if seq_end != der.len() {
        return Err(CryptoError::InvalidInput);
    }
    let mut pos = body_start;
    let r = der_read_integer(der, &mut pos, byte_len)?;
    let s = der_read_integer(der, &mut pos, byte_len)?;
    if pos != seq_end {
        return Err(CryptoError::InvalidInput);
    }
    Ok((r, s))
}

fn der_length(bytes: &[u8]) -> Result<(usize, usize)> {
    if bytes.is_empty() {
        return Err(CryptoError::InvalidInput);
    }
    let first = bytes[0];
    if first & 0x80 == 0 {
        Ok((first as usize, 0))
    }
    else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 || bytes.len() < 1 + count {
            return Err(CryptoError::InvalidInput);
        }
        let mut length = 0usize;
        for &byte in &bytes[1..1 + count] {
            length = (length << 8) | byte as usize;
        }
        Ok((length, count))
    }
}

fn der_read_integer(der: &[u8], pos: &mut usize, byte_len: usize) -> Result<Vec<u8>> {
    if der.len() < *pos + 1 || der[*pos] != 0x02 {
        return Err(CryptoError::InvalidInput);
    }
    let (length, len_bytes) = der_length(&der[*pos + 1..])?;
    let start = *pos + 1 + len_bytes + 1;
    let end = start + length;
    if end > der.len() {
        return Err(CryptoError::InvalidInput);
    }
    let raw = &der[start..end];
    if raw.is_empty() {
        return Err(CryptoError::InvalidInput);
    }
    let mut value = raw;
    if value.len() > 1 && value[0] == 0x00 {
        value = &value[1..];
    }
    if value.len() > byte_len {
        return Err(CryptoError::InvalidInput);
    }
    let mut out = vec![0u8; byte_len];
    out[byte_len - value.len()..].copy_from_slice(value);
    *pos = end;
    Ok(out)
}

fn signature_to_der<const N: usize>(sig: &Signature<N>) -> Vec<u8> {
    let r_enc = der_encode_integer(&sig.r.to_be_bytes());
    let s_enc = der_encode_integer(&sig.s.to_be_bytes());
    let body_len = r_enc.len() + s_enc.len();
    let mut out = Vec::with_capacity(2 + body_len);
    out.push(0x30);
    if body_len < 128 {
        out.push(body_len as u8);
    }
    else {
        out.push(0x81);
        out.push(body_len as u8);
    }
    out.extend_from_slice(&r_enc);
    out.extend_from_slice(&s_enc);
    out
}

fn signature_to_fixed<const N: usize>(sig: &Signature<N>, byte_len: usize) -> Vec<u8> {
    let r = sig.r.to_be_bytes();
    let s = sig.s.to_be_bytes();
    let mut out = Vec::with_capacity(byte_len * 2);
    out.extend_from_slice(&r[r.len() - byte_len..]);
    out.extend_from_slice(&s[s.len() - byte_len..]);
    out
}

fn parse_sec1<const N: usize>(bytes: &[u8], curve: &Curve<N>) -> Result<(UInt<N>, UInt<N>)> {
    let byte_len = curve.byte_len;
    match bytes.first().copied() {
        Some(0x04) if bytes.len() == 1 + 2 * byte_len => {
            let x = UInt::from_be_bytes(&bytes[1..1 + byte_len]).ok_or(CryptoError::InvalidInput)?;
            let y = UInt::from_be_bytes(&bytes[1 + byte_len..]).ok_or(CryptoError::InvalidInput)?;
            Ok((x, y))
        }
        Some(tag @ (0x02 | 0x03)) if bytes.len() == 1 + byte_len => {
            let x = UInt::from_be_bytes(&bytes[1..]).ok_or(CryptoError::InvalidInput)?;
            let y = decompress_y(x, tag == 0x03, curve)?;
            Ok((x, y))
        }
        _ => Err(CryptoError::InvalidInput),
    }
}

fn decompress_y<const N: usize>(x: UInt<N>, y_bit: bool, curve: &Curve<N>) -> Result<UInt<N>> {
    let field = Field::new(curve.p);
    let x2 = mod_mul(x, x, &field);
    let x3 = mod_mul(x2, x, &field);
    let mut three = UInt::ZERO;
    three.0[0] = 3;
    let b = UInt::from_be_bytes(curve.b).ok_or(CryptoError::InvalidInput)?;
    let rhs = mod_add(mod_sub(x3, mod_mul(three, x, &field), &curve.p), b, &curve.p);
    let mut exp = UInt(curve.p);
    let mut one = UInt::ZERO;
    one.0[0] = 1;
    exp = mod_add(exp, one, &curve.p);
    for _ in 0..2 {
        let mut carry = 0u64;
        for limb in (0..N).rev() {
            let new_carry = exp.0[limb] & 1;
            exp.0[limb] = (exp.0[limb] >> 1) | (carry << 63);
            carry = new_carry;
        }
    }
    let mut y = mod_pow(rhs, &exp, &field);
    if mod_mul(y, y, &field) != rhs {
        return Err(CryptoError::InvalidInput);
    }
    if (y.0[0] & 1 == 1) != y_bit {
        y = mod_sub(UInt(curve.p), y, &curve.p);
    }
    Ok(y)
}

fn encode_sec1<const N: usize>(x: UInt<N>, y: UInt<N>, byte_len: usize) -> Vec<u8> {
    let xb = x.to_be_bytes();
    let yb = y.to_be_bytes();
    let mut out = Vec::with_capacity(1 + 2 * byte_len);
    out.push(0x04);
    out.extend_from_slice(&xb[xb.len() - byte_len..]);
    out.extend_from_slice(&yb[yb.len() - byte_len..]);
    out
}

fn on_curve<const N: usize>(x: UInt<N>, y: UInt<N>, curve: &Curve<N>) -> bool {
    if x.is_zero() && y.is_zero() {
        return false;
    }
    if ge(&x.0, &curve.p) || ge(&y.0, &curve.p) {
        return false;
    }
    let field = Field::new(curve.p);
    let lhs = mod_mul(y, y, &field);
    let x3 = mod_mul(mod_mul(x, x, &field), x, &field);
    let mut three = UInt::ZERO;
    three.0[0] = 3;
    let b = match UInt::from_be_bytes(curve.b) {
        Some(b) => b,
        None => return false,
    };
    let rhs = mod_add(mod_sub(x3, mod_mul(three, x, &field), &curve.p), b, &curve.p);
    lhs == rhs
}

fn generate_scalar<const N: usize>(curve: &Curve<N>) -> Result<UInt<N>> {
    for _ in 0..8 {
        let mut bytes = vec![0u8; curve.byte_len];
        fill_host(&mut bytes)?;
        let candidate = UInt::from_be_bytes(&bytes).ok_or(CryptoError::InvalidInput)?;
        if !candidate.is_zero() && !ge(&candidate.0, &curve.n) {
            return Ok(candidate);
        }
    }
    Err(CryptoError::InvalidInput)
}

macro_rules! ecdsa_curve {
    ($modname:ident, $curve:expr, $byte_len:literal, $fixed_len:literal, $hash:path, $hmac:path) => {
        /// ECDSA 曲线实现。
        pub mod $modname {
            use super::*;

            const CURVE: Curve<{ $byte_len / 8 }> = $curve;
            type U = UInt<{ $byte_len / 8 }>;
            type Sig = Signature<{ $byte_len / 8 }>;

            /// 签名私钥。
            pub struct SigningKey(U);

            impl SigningKey {
                /// 用宿主安全熵生成新私钥。
                pub fn generate() -> Result<Self> {
                    generate_scalar(&CURVE).map(Self)
                }

                /// 从标量字节恢复私钥。
                pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
                    if bytes.len() != $byte_len {
                        return Err(CryptoError::InvalidInput);
                    }
                    let scalar = U::from_be_bytes(bytes).ok_or(CryptoError::InvalidInput)?;
                    if scalar.is_zero() || ge(&scalar.0, &CURVE.n) {
                        return Err(CryptoError::InvalidInput);
                    }
                    Ok(Self(scalar))
                }

                /// 对消息签名，返回 DER 编码。
                pub fn sign_der(&self, message: &[u8]) -> Vec<u8> {
                    let sig = ecdsa_sign(self.0, message, &CURVE, $hash, $hmac);
                    signature_to_der(&sig)
                }

                /// 对消息签名，返回 fixed-width（r ‖ s）。
                pub fn sign_fixed(&self, message: &[u8]) -> [u8; $fixed_len] {
                    let sig = ecdsa_sign(self.0, message, &CURVE, $hash, $hmac);
                    let fixed = signature_to_fixed(&sig, $byte_len);
                    let mut out = [0u8; $fixed_len];
                    out.copy_from_slice(&fixed);
                    out
                }

                /// 派生验签公钥。
                pub fn verifying_key(&self) -> VerifyingKey {
                    let field = Field::new(CURVE.p);
                    let point = CURVE.generator().scalar_mul(self.0, &field);
                    let (x, y) = point.affine(&field).expect("public key derivation");
                    VerifyingKey(x, y)
                }
            }

            /// 验签公钥。
            #[derive(Clone, Copy)]
            pub struct VerifyingKey(U, U);

            impl VerifyingKey {
                /// 从 SEC1 编码（压缩或非压缩）解析公钥。
                pub fn from_sec1_bytes(bytes: &[u8]) -> Result<Self> {
                    let (x, y) = parse_sec1(bytes, &CURVE)?;
                    if !on_curve(x, y, &CURVE) {
                        return Err(CryptoError::InvalidInput);
                    }
                    Ok(Self(x, y))
                }

                /// 导出非压缩 SEC1 编码。
                pub fn to_sec1_bytes(&self) -> Vec<u8> {
                    encode_sec1(self.0, self.1, $byte_len)
                }

                /// 验证 DER 编码签名。
                pub fn verify_der(&self, message: &[u8], signature: &[u8]) -> Result<()> {
                    let (r, s) = der_decode_signature(signature, $byte_len)?;
                    let r = U::from_be_bytes(&r).ok_or(CryptoError::InvalidInput)?;
                    let s = U::from_be_bytes(&s).ok_or(CryptoError::InvalidInput)?;
                    self.verify(message, &Sig { r, s })
                }

                /// 验证 fixed-width 签名。
                pub fn verify_fixed(&self, message: &[u8], signature: &[u8]) -> Result<()> {
                    if signature.len() != $fixed_len {
                        return Err(CryptoError::InvalidInput);
                    }
                    let r = U::from_be_bytes(&signature[..$byte_len]).ok_or(CryptoError::InvalidInput)?;
                    let s = U::from_be_bytes(&signature[$byte_len..]).ok_or(CryptoError::InvalidInput)?;
                    self.verify(message, &Sig { r, s })
                }

                fn verify(&self, message: &[u8], sig: &Sig) -> Result<()> {
                    if ecdsa_verify(&(self.0, self.1), message, sig, &CURVE, $hash) { Ok(()) } else { Err(CryptoError::Authentication) }
                }
            }

            /// DER 转 fixed-width。
            pub fn der_to_fixed(der: &[u8]) -> Result<[u8; $fixed_len]> {
                let (r, s) = der_decode_signature(der, $byte_len)?;
                let mut out = [0u8; $fixed_len];
                out[..$byte_len].copy_from_slice(&r);
                out[$byte_len..].copy_from_slice(&s);
                Ok(out)
            }

            /// fixed-width 转 DER。
            pub fn fixed_to_der(fixed: &[u8]) -> Result<Vec<u8>> {
                if fixed.len() != $fixed_len {
                    return Err(CryptoError::InvalidInput);
                }
                let r = U::from_be_bytes(&fixed[..$byte_len]).ok_or(CryptoError::InvalidInput)?;
                let s = U::from_be_bytes(&fixed[$byte_len..]).ok_or(CryptoError::InvalidInput)?;
                Ok(signature_to_der(&Sig { r, s }))
            }
        }
    };
}

ecdsa_curve!(p256, P256, 32, 64, crate::hash::sha256, hmac_sha256_bytes);
ecdsa_curve!(p384, P384, 48, 96, crate::hash::sha384, hmac_sha384_bytes);

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn tiny_field_montgomery() {
        // m = 2^64 - 1 (N=1)，R = 2^64，m' = 1。
        let m: [u64; 1] = [0xFFFFFFFFFFFFFFFF];
        let field = Field::new(m);
        assert_eq!(field.m_prime, 1);
        // R^2 mod m = (2^64)^2 mod (2^64-1) = 1^2 = 1 mod (2^64-1) = 1
        assert_eq!(field.r2.0[0], 1);

        let mut a = UInt::<1>::ZERO;
        a.0[0] = 7;
        let mut b = UInt::<1>::ZERO;
        b.0[0] = 8;
        let c = mod_mul(a, b, &field);
        // 7*8 = 56 mod (2^64-1) = 56
        assert_eq!(c.0[0], 56);
    }

    #[test]
    fn p256_der_debug() {
        let message = b"boost-crypto ecdsa p-256 fixed message";
        let key = p256::SigningKey::generate().expect("host entropy");
        let der = key.sign_der(message);
        println!("DER len={} bytes={:02x?}", der.len(), der);
        let (r, s) = der_decode_signature(&der, 32).expect("decode");
        println!("r len={} s len={}", r.len(), s.len());
    }

    #[test]
    fn p256_field_and_point_smoke() {
        let field = Field::new(P256.p);
        // 2 * 3 = 6 mod p
        let mut two = UInt::<4>::ZERO;
        two.0[0] = 2;
        let mut three = UInt::<4>::ZERO;
        three.0[0] = 3;
        let six = mod_mul(two, three, &field);
        assert_eq!(six.0[0], 6);
        assert!(six.0[1..].iter().all(|x| *x == 0));

        // scalar_mul(G, 1) == G
        let mut one = UInt::<4>::ZERO;
        one.0[0] = 1;
        let g = P256.generator();
        let g1 = g.scalar_mul(one, &field);
        let (x, y) = g1.affine(&field).expect("scalar 1");
        assert_eq!(x.0, P256.gx);
        assert_eq!(y.0, P256.gy);

        // 2G via double equals scalar_mul(G, 2)
        let g2_double = g.double(&field).affine(&field).expect("double G");
        let mut two = UInt::<4>::ZERO;
        two.0[0] = 2;
        let g2_scalar = g.scalar_mul(two, &field).affine(&field).expect("scalar 2");
        assert_eq!(g2_double.0.0, g2_scalar.0.0);
        assert_eq!(g2_double.1.0, g2_scalar.1.0);

        // G is on the curve and n*G is infinity.
        assert!(on_curve(UInt(P256.gx), UInt(P256.gy), &P256));
        assert!(P256.generator().scalar_mul(UInt(P256.n), &field).is_identity());
    }

    #[test]
    fn p384_field_and_point_smoke() {
        let field = Field::new(P384.p);
        assert!(on_curve(UInt(P384.gx), UInt(P384.gy), &P384));
        let mut two = UInt::<6>::ZERO;
        two.0[0] = 2;
        let g = P384.generator();
        let g2_double = g.double(&field).affine(&field).expect("double G");
        let g2_scalar = g.scalar_mul(two, &field).affine(&field).expect("scalar 2");
        assert_eq!(g2_double.0.0, g2_scalar.0.0);
        assert_eq!(g2_double.1.0, g2_scalar.1.0);
        assert!(g.scalar_mul(UInt(P384.n), &field).is_identity());
    }

    fn schoolbook_mul<const N: usize>(a: &[u64; N], b: &[u64; N]) -> Vec<u64> {
        let mut out = vec![0u64; 2 * N + 1];
        for i in 0..N {
            let mut carry = 0u128;
            for j in 0..N {
                let value = out[i + j] as u128 + (a[i] as u128) * (b[j] as u128) + carry;
                out[i + j] = value as u64;
                carry = value >> 64;
            }
            let mut k = i + N;
            while carry > 0 {
                let value = out[k] as u128 + carry;
                out[k] = value as u64;
                carry = value >> 64;
                k += 1;
            }
        }
        out.truncate(2 * N);
        out
    }

    #[test]
    fn p384_montgomery_vs_schoolbook() {
        let field = Field::new(P384.p);
        let samples = [
            UInt::<6>(P384.gx),
            UInt::<6>(P384.gy),
            UInt::<6>(P384.p),
            mod_sub(UInt(P384.p), UInt::<6>(P384.gx), &P384.p),
            UInt::one(),
            UInt::ZERO,
        ];
        for a in samples {
            for b in samples {
                let mont = mod_mul(a, b, &field);
                let refv = rem_mod(&schoolbook_mul(&a.0, &b.0), &P384.p);
                assert_eq!(mont.0, refv.0, "mont mismatch for a={:016x?}", a.0);
            }
        }
    }

    #[test]
    fn p384_generator_on_curve_reference() {
        // 完全绕开 Montgomery：学校乘法 + rem_mod 直接验证 G 是否满足 y^2 = x^3 - 3x + b。
        let gx = UInt::<6>(P384.gx);
        let gy = UInt::<6>(P384.gy);
        let b: UInt<6> = UInt::from_be_bytes(P384_B).expect("b fits");
        let x2 = rem_mod(&schoolbook_mul(&gx.0, &gx.0), &P384.p);
        let x3 = rem_mod(&schoolbook_mul(&x2.0, &gx.0), &P384.p);
        let lhs = rem_mod(&schoolbook_mul(&gy.0, &gy.0), &P384.p);
        let mut three = UInt::<6>::ZERO;
        three.0[0] = 3;
        let three_x = rem_mod(&schoolbook_mul(&three.0, &gx.0), &P384.p);
        let rhs = rem_mod(&zero_extend(mod_sub(mod_sub(x3, three_x, &P384.p), UInt(P384.p), &P384.p)).to_vec(), &P384.p);
        let rhs = mod_add(rem_mod(&zero_extend(b), &P384.p), rhs, &P384.p);
        assert_eq!(lhs.0, rhs.0, "G not on curve by reference math");
    }
}
