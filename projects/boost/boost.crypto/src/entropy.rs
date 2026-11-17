//! 宿主安全熵到第三方原语 RNG trait 的适配。
//!
//! 生产密钥的熵只来自 `boost-random` 的 `Secure<SystemEntropy>` 合同：无 seed、无可重放状态、
//! 无确定性回退。本模块把该合同适配到 `rsa`、`p256`、`p384`、`x25519-dalek` 要求的
//! `rand_core` 0.6 trait，本 crate 不直接请求平台随机接口。

use boost_random::{Secure, SystemEntropy};
use rand_core::{CryptoRng, Error as RandError, RngCore};

/// 包装宿主安全熵句柄，实现 `rand_core` 0.6 的填充接口。
pub(crate) struct HostEntropy(Secure<SystemEntropy>);

impl HostEntropy {
    /// 创建宿主熵句柄。构造本身不探测平台，首次非空请求才探测。
    pub(crate) fn new() -> Self {
        Self(Secure::system())
    }

    /// 用宿主熵填充 `dst`，失败映射为 [`CryptoError::Entropy`]。
    pub(crate) fn fill(&mut self, dst: &mut [u8]) -> crate::Result<()> {
        self.0.fill(dst).map_err(crate::CryptoError::Entropy)
    }

    /// 在调用不可失败的原语构造器之前显式探测熵源，使熵缺失以错误而非 panic 报告。
    pub(crate) fn probe(&mut self) -> crate::Result<()> {
        self.fill(&mut [0u8; 1])
    }
}

impl RngCore for HostEntropy {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.try_fill_bytes(dst).expect("host entropy failed after a successful probe");
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), RandError> {
        self.0.fill(dst).map_err(RandError::new)
    }
}

impl CryptoRng for HostEntropy {}
