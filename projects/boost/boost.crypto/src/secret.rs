//! 秘密材料的生命周期类型。
//!
//! [`SecretBytes`] 是密码路径上所有私钥、共享秘密与派生密钥的载体：内存清零由 `Zeroize` 与
//! `ZeroizeOnDrop` 保证，`Debug` 输出打码，不提供 `Deref`、`AsRef`、`Clone` 或 `PartialEq`，
//! 读取明文必须显式调用 [`SecretBytes::expose`]，让泄漏点在代码里可见。

use core::fmt::{self, Debug, Formatter};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// 会在离开作用域时清零的秘密字节。
///
/// 内部以 `Zeroizing<Vec<u8>>` 持有：即使本类型自身的清零逻辑被绕过，堆缓冲也会在 drop 时
/// 被 `Zeroizing` 清零。
#[derive(ZeroizeOnDrop)]
pub struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    /// 从已有字节构造，立即接管其生命周期。
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// 秘密字节的长度。
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 显式读取明文字节。
    ///
    /// 这是唯一的读取入口：调用它意味着调用方承认此处将暴露秘密，应尽快把数据交给原语并让
    /// 副本随作用域结束。
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Zeroize for SecretBytes {
    fn zeroize(&mut self) {
        self.0.as_mut_slice().zeroize();
    }
}

impl Debug for SecretBytes {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretBytes").field("len", &self.0.len()).field("value", &"[redacted]").finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_content() {
        let secret = SecretBytes::new(b"super-secret-material".to_vec());
        let text = format!("{secret:?}");
        assert!(!text.contains("super"));
        assert!(!text.contains("secret-material"));
        assert!(text.contains("redacted"));
        assert_eq!(secret.len(), 21);
    }

    #[test]
    fn expose_returns_the_original_bytes() {
        let secret = SecretBytes::new(vec![0xAA; 8]);
        assert_eq!(secret.expose(), &[0xAAu8; 8]);
    }

    #[test]
    fn zeroize_clears_in_place() {
        let mut secret = SecretBytes::new(vec![0xBB; 16]);
        secret.zeroize();
        assert!(secret.expose().iter().all(|&b| b == 0));
    }
}
