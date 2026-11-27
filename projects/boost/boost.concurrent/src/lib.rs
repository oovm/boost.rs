//! Boost 家族的并发原语。
//!
//! 只提供扫描、编译流水线这类 CPU 密集消费者需要的最小合同：
//! [`Cancel`] 协作式取消、[`Waiter`] 计数等待组、[`Pool`] 固定线程执行器。
//! 不引入全局运行时，也不做工作窃取；需要借用数据、按输入序合并的
//! 作用域并行归 `boost-parallel`。
//!
//! 线程数统一由 [`threads`] 计算：显式请求按硬件并发度裁剪，`0` 表示取硬件
//! 并发度。

#![doc = include_str!("readme.md")]
#![deny(missing_docs)]

mod cancel;
mod pool;
mod waiter;

pub use cancel::Cancel;
pub use pool::Pool;
pub use waiter::Waiter;

/// 返回实际使用的工作线程数。
///
/// `requested` 为 `0` 时取硬件并发度；其余情况不超过硬件并发度，避免 CPU
/// 密集任务把机器压满后互相抢占。
pub fn threads(requested: usize) -> usize {
    let available = std::thread::available_parallelism().map(|count| count.get()).unwrap_or(1);
    match requested {
        0 => available,
        wanted => wanted.min(available),
    }
}
