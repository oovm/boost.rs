//! 宿主安全熵入口。
//!
//! 生产密钥的熵只来自 `boost-random` 的 `Secure<SystemEntropy>` 合同：无 seed、无可重放状态、
//! 无确定性回退。本模块只暴露逐次可失败的填充函数，不实现外部 RNG trait，也不在失败时 panic、
//! 零填充、返回伪成功或回退普通 PRNG。

use boost_random::Secure;

use crate::{CryptoError, Result};

/// 用宿主安全熵填充 `dst`。
///
/// 每次调用都可能失败。错误按 [`CryptoError::Entropy`] 原样传回，调用方不得把失败当作成功，
/// 也不得在本地重试之外假设后续读取一定成功。
pub(crate) fn fill_host(dst: &mut [u8]) -> Result<()> {
    Secure::system().fill(dst).map_err(CryptoError::Entropy)
}
