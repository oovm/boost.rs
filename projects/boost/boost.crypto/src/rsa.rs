//! RSA 公钥运算：PKCS#1 v1.5 / PSS 验签与 OAEP(SHA-1) 加密。
//!
//! 私钥生成与随机化签名需要外部 RNG trait，本 crate 不再提供；需要签名时由上层用自有无外部
//! 运行时的实现补齐。验签只消费公钥与字节，没有随机性。OAEP 加密的填充由本模块自行实现，
//! 随机种子逐次取自 `boost-random` 宿主熵，失败按 [`CryptoError::Entropy`] 传回，不返回密文。
//!
//! 摘要类型用 `sha2-rsa`（package `sha2` 0.10 世代别名）与 `sha1-rsa` 命名，以匹配 `rsa` 0.9
//! 的 digest trait 世代；对外接口只收发字节。

use crate::{CryptoError, Result, entropy::fill_host};
use rsa::{
    BigUint, RsaPublicKey, pkcs1v15::VerifyingKey as Pkcs1VerifyingKey, pss::VerifyingKey as PssVerifyingKey, signature::Verifier as _,
    traits::PublicKeyParts,
};
use sha1_rsa::{Digest as _, Sha1};
use sha2_rsa::{Sha256, Sha384};

/// OAEP 使用的摘要长度（SHA-1）。
const OAEP_HASH_LEN: usize = 20;

/// RSA 公钥。
pub struct PublicKey(RsaPublicKey);

impl PublicKey {
    /// 从 SubjectPublicKeyInfo DER 解析公钥。
    pub fn from_spki_der(bytes: &[u8]) -> Result<Self> {
        use rsa::pkcs8::DecodePublicKey as _;
        RsaPublicKey::from_public_key_der(bytes).map(Self).map_err(|_| CryptoError::InvalidInput)
    }

    /// 从 PEM 文本解析公钥。
    pub fn from_pem(text: &str) -> Result<Self> {
        use rsa::pkcs8::DecodePublicKey as _;
        RsaPublicKey::from_public_key_pem(text).map(Self).map_err(|_| CryptoError::InvalidInput)
    }

    /// PKCS#1 v1.5 + SHA-256 验签。
    pub fn verify_pkcs1v15_sha256(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = Pkcs1VerifyingKey::<Sha256>::new(self.0.clone());
        let signature = rsa::pkcs1v15::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }

    /// PKCS#1 v1.5 + SHA-384 验签。
    pub fn verify_pkcs1v15_sha384(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = Pkcs1VerifyingKey::<Sha384>::new(self.0.clone());
        let signature = rsa::pkcs1v15::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }

    /// PSS + SHA-256 验签。
    pub fn verify_pss_sha256(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = PssVerifyingKey::<Sha256>::new(self.0.clone());
        let signature = rsa::pss::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }

    /// PSS + SHA-384 验签。
    pub fn verify_pss_sha384(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = PssVerifyingKey::<Sha384>::new(self.0.clone());
        let signature = rsa::pss::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }

    /// RSAES-OAEP 加密，摘要与 MGF1 均用 SHA-1（与 MySQL `caching_sha2_password` 一致）。
    ///
    /// 填充由本模块构造，OAEP 种子逐次取自宿主安全熵。熵失败或明文过长都不返回密文。
    pub fn encrypt_oaep_sha1(&self, message: &[u8]) -> Result<Vec<u8>> {
        let bits = self.0.n().bits();
        let k = bits.div_ceil(8);
        let db_len = k.checked_sub(OAEP_HASH_LEN + 1).ok_or(CryptoError::InvalidInput)?;
        let max_message = db_len.checked_sub(OAEP_HASH_LEN + 1).ok_or(CryptoError::InvalidInput)?;
        if message.len() > max_message {
            return Err(CryptoError::InvalidInput);
        }

        let mut db = Vec::with_capacity(db_len);
        db.extend_from_slice(&Sha1::digest(b""));
        let ps_len = db_len - message.len() - OAEP_HASH_LEN - 1;
        db.resize(db.len() + ps_len, 0);
        db.push(0x01);
        db.extend_from_slice(message);

        let mut seed = [0u8; OAEP_HASH_LEN];
        fill_host(&mut seed)?;

        let db_mask = mgf1_sha1(&seed, db_len);
        let mut masked_db = db;
        for (slot, mask) in masked_db.iter_mut().zip(db_mask.iter()) {
            *slot ^= mask;
        }
        let seed_mask = mgf1_sha1(&masked_db, OAEP_HASH_LEN);
        let mut masked_seed = seed;
        for (slot, mask) in masked_seed.iter_mut().zip(seed_mask.iter()) {
            *slot ^= mask;
        }

        let mut encoded = Vec::with_capacity(k);
        encoded.push(0x00);
        encoded.extend_from_slice(&masked_seed);
        encoded.extend_from_slice(&masked_db);

        let value = BigUint::from_bytes_be(&encoded);
        let cipher = value.modpow(self.0.e(), self.0.n());
        let bytes = cipher.to_bytes_be();
        if bytes.len() > k {
            return Err(CryptoError::InvalidInput);
        }
        let mut out = vec![0u8; k - bytes.len()];
        out.extend_from_slice(&bytes);
        Ok(out)
    }
}

/// `MGF1` over SHA-1（RFC 8017 附录 B.2.1）。
fn mgf1_sha1(seed: &[u8], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length + OAEP_HASH_LEN);
    let mut counter: u32 = 0;
    while out.len() < length {
        let mut digest = Sha1::new();
        digest.update(seed);
        digest.update(counter.to_be_bytes());
        out.extend_from_slice(&digest.finalize());
        counter = counter.wrapping_add(1);
    }
    out.truncate(length);
    out
}
