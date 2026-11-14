//! Performance measurement with explicit samples, warmup, and structured reports.

#[macro_use]
mod macros;

pub mod bencher;
pub mod config;
pub mod group;
pub mod report;
pub mod suite;

pub use bencher::Bencher;
pub use config::BenchmarkConfig;
pub use group::BenchmarkGroup;
pub use report::{BenchmarkStats, Sample, render_text};
pub use suite::BenchmarkSuite;

/// Prevents the compiler from optimizing away benchmark inputs and outputs.
pub use core::hint::black_box;
