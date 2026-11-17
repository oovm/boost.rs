//! ECDSA 签名与验签（NIST P-256 与 P-384）。
//!
//! 签名输出同时提供 DER 与 fixed-width（r ‖ s）两种编码，并可互转。签名是 RFC 6979 确定性生成，
//! 私钥生成走宿主熵。本模块只做原语执行，不承载协议层的密钥编码语义。

/// ECDSA/P-256。
pub mod p256 {
    use crate::{CryptoError, Result, entropy::fill_host};
    use ::p256::ecdsa::{self, SigningKey as InnerSigningKey, VerifyingKey as InnerVerifyingKey};
    use signature::{Signer as _, Verifier as _};

    /// P-256 签名私钥。
    pub struct SigningKey(InnerSigningKey);

    impl SigningKey {
        /// 用宿主安全熵生成新私钥。
        ///
        /// 熵读取失败按 [`CryptoError::Entropy`] 传回。标量落在合法区间外时有限重试，
        /// 超出预算返回 [`CryptoError::InvalidInput`]。
        pub fn generate() -> Result<Self> {
            for _ in 0..8 {
                let mut bytes = [0u8; 32];
                fill_host(&mut bytes)?;
                if let Ok(key) = InnerSigningKey::from_slice(&bytes) {
                    return Ok(Self(key));
                }
            }
            Err(CryptoError::InvalidInput)
        }

        /// 从 32 字节标量恢复私钥。
        pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
            InnerSigningKey::from_slice(bytes).map(Self).map_err(|_| CryptoError::InvalidInput)
        }

        /// 对消息签名，返回 DER 编码。
        pub fn sign_der(&self, message: &[u8]) -> Vec<u8> {
            let signature: ecdsa::Signature = self.0.sign(message);
            signature.to_der().as_bytes().to_vec()
        }

        /// 对消息签名，返回 64 字节 fixed-width（r ‖ s）。
        pub fn sign_fixed(&self, message: &[u8]) -> [u8; 64] {
            let signature: ecdsa::Signature = self.0.sign(message);
            let bytes = signature.to_bytes();
            let mut out = [0u8; 64];
            out.copy_from_slice(bytes.as_slice());
            out
        }

        /// 派生验签公钥。
        pub fn verifying_key(&self) -> VerifyingKey {
            VerifyingKey(InnerVerifyingKey::from(&self.0))
        }
    }

    /// P-256 验签公钥。
    pub struct VerifyingKey(InnerVerifyingKey);

    impl VerifyingKey {
        /// 从 SEC1 编码（压缩或非压缩）解析公钥。
        pub fn from_sec1_bytes(bytes: &[u8]) -> Result<Self> {
            InnerVerifyingKey::from_sec1_bytes(bytes).map(Self).map_err(|_| CryptoError::InvalidInput)
        }

        /// 导出非压缩 SEC1 编码（65 字节）。
        pub fn to_sec1_bytes(&self) -> Vec<u8> {
            self.0.to_encoded_point(false).as_bytes().to_vec()
        }

        /// 验证 DER 编码签名。
        pub fn verify_der(&self, message: &[u8], signature: &[u8]) -> Result<()> {
            let signature = ecdsa::Signature::from_der(signature).map_err(|_| CryptoError::InvalidInput)?;
            self.0.verify(message, &signature).map_err(|_| CryptoError::Authentication)
        }

        /// 验证 64 字节 fixed-width 签名。
        pub fn verify_fixed(&self, message: &[u8], signature: &[u8]) -> Result<()> {
            let signature = ecdsa::Signature::from_slice(signature).map_err(|_| CryptoError::InvalidInput)?;
            self.0.verify(message, &signature).map_err(|_| CryptoError::Authentication)
        }
    }

    /// DER 转 fixed-width（64 字节）。
    pub fn der_to_fixed(der: &[u8]) -> Result<[u8; 64]> {
        let signature = ecdsa::Signature::from_der(der).map_err(|_| CryptoError::InvalidInput)?;
        let bytes = signature.to_bytes();
        let mut out = [0u8; 64];
        out.copy_from_slice(bytes.as_slice());
        Ok(out)
    }

    /// fixed-width（64 字节）转 DER。
    pub fn fixed_to_der(fixed: &[u8]) -> Result<Vec<u8>> {
        let signature = ecdsa::Signature::from_slice(fixed).map_err(|_| CryptoError::InvalidInput)?;
        Ok(signature.to_der().as_bytes().to_vec())
    }
}

/// ECDSA/P-384。
pub mod p384 {
    use crate::{CryptoError, Result, entropy::fill_host};
    use ::p384::ecdsa::{self, SigningKey as InnerSigningKey, VerifyingKey as InnerVerifyingKey};
    use signature::{Signer as _, Verifier as _};

    /// P-384 签名私钥。
    pub struct SigningKey(InnerSigningKey);

    impl SigningKey {
        /// 用宿主安全熵生成新私钥。
        ///
        /// 熵读取失败按 [`CryptoError::Entropy`] 传回。标量落在合法区间外时有限重试，
        /// 超出预算返回 [`CryptoError::InvalidInput`]。
        pub fn generate() -> Result<Self> {
            for _ in 0..8 {
                let mut bytes = [0u8; 48];
                fill_host(&mut bytes)?;
                if let Ok(key) = InnerSigningKey::from_slice(&bytes) {
                    return Ok(Self(key));
                }
            }
            Err(CryptoError::InvalidInput)
        }

        /// 从 48 字节标量恢复私钥。
        pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
            InnerSigningKey::from_slice(bytes).map(Self).map_err(|_| CryptoError::InvalidInput)
        }

        /// 对消息签名，返回 DER 编码。
        pub fn sign_der(&self, message: &[u8]) -> Vec<u8> {
            let signature: ecdsa::Signature = self.0.sign(message);
            signature.to_der().as_bytes().to_vec()
        }

        /// 对消息签名，返回 96 字节 fixed-width（r ‖ s）。
        pub fn sign_fixed(&self, message: &[u8]) -> [u8; 96] {
            let signature: ecdsa::Signature = self.0.sign(message);
            let bytes = signature.to_bytes();
            let mut out = [0u8; 96];
            out.copy_from_slice(bytes.as_slice());
            out
        }

        /// 派生验签公钥。
        pub fn verifying_key(&self) -> VerifyingKey {
            VerifyingKey(InnerVerifyingKey::from(&self.0))
        }
    }

    /// P-384 验签公钥。
    pub struct VerifyingKey(InnerVerifyingKey);

    impl VerifyingKey {
        /// 从 SEC1 编码（压缩或非压缩）解析公钥。
        pub fn from_sec1_bytes(bytes: &[u8]) -> Result<Self> {
            InnerVerifyingKey::from_sec1_bytes(bytes).map(Self).map_err(|_| CryptoError::InvalidInput)
        }

        /// 导出非压缩 SEC1 编码（97 字节）。
        pub fn to_sec1_bytes(&self) -> Vec<u8> {
            self.0.to_encoded_point(false).as_bytes().to_vec()
        }

        /// 验证 DER 编码签名。
        pub fn verify_der(&self, message: &[u8], signature: &[u8]) -> Result<()> {
            let signature = ecdsa::Signature::from_der(signature).map_err(|_| CryptoError::InvalidInput)?;
            self.0.verify(message, &signature).map_err(|_| CryptoError::Authentication)
        }

        /// 验证 96 字节 fixed-width 签名。
        pub fn verify_fixed(&self, message: &[u8], signature: &[u8]) -> Result<()> {
            let signature = ecdsa::Signature::from_slice(signature).map_err(|_| CryptoError::InvalidInput)?;
            self.0.verify(message, &signature).map_err(|_| CryptoError::Authentication)
        }
    }

    /// DER 转 fixed-width（96 字节）。
    pub fn der_to_fixed(der: &[u8]) -> Result<[u8; 96]> {
        let signature = ecdsa::Signature::from_der(der).map_err(|_| CryptoError::InvalidInput)?;
        let bytes = signature.to_bytes();
        let mut out = [0u8; 96];
        out.copy_from_slice(bytes.as_slice());
        Ok(out)
    }

    /// fixed-width（96 字节）转 DER。
    pub fn fixed_to_der(fixed: &[u8]) -> Result<Vec<u8>> {
        let signature = ecdsa::Signature::from_slice(fixed).map_err(|_| CryptoError::InvalidInput)?;
        Ok(signature.to_der().as_bytes().to_vec())
    }
}
