//! RSA 签名与验签（PKCS#1 v1.5 与 PSS）。
//!
//! keygen 与 PSS 签名的盐值都走 `boost-random` 宿主熵。摘要类型用 `sha2-rsa`（package `sha2`
//! 0.10 世代别名）命名，以匹配 `rsa` 0.9 的 digest trait 世代；对外接口只收发字节。

use crate::{CryptoError, Result, entropy::HostEntropy};
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1v15::{self, VerifyingKey as Pkcs1VerifyingKey},
    pss::{self, VerifyingKey as PssVerifyingKey},
};
use sha2_rsa::{Sha256, Sha384};
use signature::{RandomizedSigner as _, SignatureEncoding as _, Signer as _, Verifier as _};

/// RSA 私钥。
pub struct PrivateKey(RsaPrivateKey);

impl PrivateKey {
    /// 用宿主安全熵生成 `bits` 位 RSA 私钥（例如 2048）。
    ///
    /// 熵缺失返回 [`CryptoError::Entropy`]；`bits` 非法返回 [`CryptoError::InvalidInput`]。
    pub fn generate(bits: usize) -> Result<Self> {
        let mut entropy = HostEntropy::new();
        entropy.probe()?;
        RsaPrivateKey::new(&mut entropy, bits).map(Self).map_err(|_| CryptoError::InvalidInput)
    }

    /// 派生公钥。
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.to_public_key())
    }

    /// PKCS#1 v1.5 + SHA-256 签名。
    pub fn sign_pkcs1v15_sha256(&self, message: &[u8]) -> Vec<u8> {
        let key = pkcs1v15::SigningKey::<Sha256>::new(self.0.clone());
        key.sign(message).to_vec()
    }

    /// PKCS#1 v1.5 + SHA-384 签名。
    pub fn sign_pkcs1v15_sha384(&self, message: &[u8]) -> Vec<u8> {
        let key = pkcs1v15::SigningKey::<Sha384>::new(self.0.clone());
        key.sign(message).to_vec()
    }

    /// PSS + SHA-256 签名，盐值与盲化用宿主熵。
    pub fn sign_pss_sha256(&self, message: &[u8]) -> Result<Vec<u8>> {
        let key = pss::SigningKey::<Sha256>::new(self.0.clone());
        let mut entropy = HostEntropy::new();
        entropy.probe()?;
        Ok(key.sign_with_rng(&mut entropy, message).to_vec())
    }

    /// PSS + SHA-384 签名，盐值与盲化用宿主熵。
    pub fn sign_pss_sha384(&self, message: &[u8]) -> Result<Vec<u8>> {
        let key = pss::SigningKey::<Sha384>::new(self.0.clone());
        let mut entropy = HostEntropy::new();
        entropy.probe()?;
        Ok(key.sign_with_rng(&mut entropy, message).to_vec())
    }
}

/// RSA 公钥。
pub struct PublicKey(RsaPublicKey);

impl PublicKey {
    /// PKCS#1 v1.5 + SHA-256 验签。
    pub fn verify_pkcs1v15_sha256(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = Pkcs1VerifyingKey::<Sha256>::new(self.0.clone());
        let signature = pkcs1v15::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }

    /// PKCS#1 v1.5 + SHA-384 验签。
    pub fn verify_pkcs1v15_sha384(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = Pkcs1VerifyingKey::<Sha384>::new(self.0.clone());
        let signature = pkcs1v15::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }

    /// PSS + SHA-256 验签。
    pub fn verify_pss_sha256(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = PssVerifyingKey::<Sha256>::new(self.0.clone());
        let signature = pss::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }

    /// PSS + SHA-384 验签。
    pub fn verify_pss_sha384(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let key = PssVerifyingKey::<Sha384>::new(self.0.clone());
        let signature = pss::Signature::try_from(signature).map_err(|_| CryptoError::InvalidInput)?;
        key.verify(message, &signature).map_err(|_| CryptoError::Authentication)
    }
}
