//! Atlas 门面。
//!
//! 只做两件事：把运行时装配层重导出为 [`wire`]，并在启用 `macro` feature 时重导出
//! `Component` derive。应用可以经门面使用，也可以直接依赖 `atlas-wire` 与 `atlas-macro`；
//! 两种方式消费同一份合同。
//!
//! ```ignore
//! use atlas::Component;
//!
//! #[derive(Component)]
//! #[atlas(input = OrderServiceInput, runtime = atlas::wire)]
//! struct OrderService {
//!     region: String,
//!     #[wire]
//!     repository: std::sync::Arc<dyn OrderRepository + Send + Sync>,
//! }
//! ```

#![deny(missing_docs)]

#[cfg(feature = "macro")]
pub use atlas_macro::Component;
pub use atlas_wire as wire;
