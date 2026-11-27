//! 协作式取消令牌。

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// 可克隆的取消令牌；任意克隆取消后，所有克隆都观察到已取消。
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// 创建未取消的令牌。
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记取消；重复调用无副作用。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// 是否已取消。
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// 未取消时执行 `body`；已取消时返回 `None`。
    pub fn run<T>(&self, body: impl FnOnce() -> T) -> Option<T> {
        if self.is_cancelled() { None } else { Some(body()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_flag() {
        let token = Cancel::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn run_skips_cancelled_work() {
        let token = Cancel::new();
        assert_eq!(token.run(|| 7), Some(7));
        token.cancel();
        assert_eq!(token.run(|| 7), None);
    }
}
