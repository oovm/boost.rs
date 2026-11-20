//! Legion 报告数据模型与文件落盘；HTML 渲染与图表岛在 `package.manager.asgard::report`。

#[doc(hidden)]
pub mod models;
#[doc(hidden)]
pub mod standalone;
#[doc(hidden)]
pub mod write;

pub use models::{BenchReport, BenchResultRow, CoverageFeatureEntry, CoverageReport, TestResultEntry};
pub use standalone::{build_standalone_html, finish_standalone_report};
pub use write::atomic_write_all_text;
