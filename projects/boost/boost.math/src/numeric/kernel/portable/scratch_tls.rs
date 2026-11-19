//! 便利包装用的 scratch 借用。
//!
//! `std` 目标复用线程局部 arena（避免每次便利调用重新分配）；`no_std` 目标
//! 没有线程局部存储，退化为每次调用新建 scratch，语义相同、分配更多。

use crate::numeric::{budget::ExecutionBudget, kernel::ScratchWorkspace};

/// 借用 scratch 执行 kernel（调用结束清空）。
#[cfg(feature = "std")]
pub(crate) fn with_kernel_scratch<R>(budget: &ExecutionBudget, f: impl FnOnce(&mut ScratchWorkspace, &ExecutionBudget) -> R) -> R {
    use std::{cell::RefCell, thread_local};

    thread_local! {
        static KERNEL_SCRATCH: RefCell<ScratchWorkspace> = RefCell::new(ScratchWorkspace::default());
    }

    KERNEL_SCRATCH.with(|cell| {
        if let Ok(mut scratch) = cell.try_borrow_mut() {
            let result = f(&mut *scratch, budget);
            scratch.clear();
            result
        }
        else {
            let mut scratch = ScratchWorkspace::default();
            f(&mut scratch, budget)
        }
    })
}

/// 借用 scratch 执行 kernel（`no_std`：每次新建 arena）。
#[cfg(not(feature = "std"))]
pub(crate) fn with_kernel_scratch<R>(budget: &ExecutionBudget, f: impl FnOnce(&mut ScratchWorkspace, &ExecutionBudget) -> R) -> R {
    let mut scratch = ScratchWorkspace::default();
    f(&mut scratch, budget)
}
