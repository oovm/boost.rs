use boost_random::SourceError;
use core::fmt::{self, Display, Formatter};

/// 密码原语层的失败分层。
///
/// 三类错误各自对应调用方能采取的不同动作：[`Entropy`](Self::Entropy) 表示宿主安全熵不可用，
/// 属于环境故障，重试或换机；[`InvalidInput`](Self::InvalidInput) 表示密钥、nonce、标签长度或
/// DER/base64 编码不合法，属于调用方数据问题；[`Authentication`](Self::Authentication) 表示
/// AEAD 标签或签名验证失败，属于对端数据不可信。秘密材料不进入错误值，错误也不携带输入内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CryptoError {
    /// 宿主安全熵源失败，携带 `boost-random` 的原始分层错误。
    Entropy(SourceError),
    /// 输入长度、编码或密钥材料不合法。
    InvalidInput,
    /// AEAD 认证解密或签名验证失败。
    Authentication,
}

impl From<SourceError> for CryptoError {
    fn from(error: SourceError) -> Self {
        Self::Entropy(error)
    }
}

impl Display for CryptoError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entropy(error) => write!(f, "the host entropy source failed: {error}"),
            Self::InvalidInput => f.write_str("the input length, encoding, or key material is invalid"),
            Self::Authentication => f.write_str("the authentication tag or signature is invalid"),
        }
    }
}

impl std::error::Error for CryptoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Entropy(error) => Some(error),
            Self::InvalidInput | Self::Authentication => None,
        }
    }
}

/// 本 crate 的统一结果别名。
pub type Result<T> = core::result::Result<T, CryptoError>;
