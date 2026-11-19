//! 隔离的 Wasm 互操作验证工具（`wasm-contract`）。
//!
//! 本 crate 自带空的 `[workspace]` 表，因此**不是** Boost workspace 的成员：它的
//! `wasmtime` 依赖不会进入 Boost 的依赖图，也不会写进仓根的 `Cargo.lock`。
//!
//! 它只服务于一类验收：把 `hardware-wasm` 构建出的 `wasm32-unknown-unknown` 产物
//! 交给 `wasmtime` 实例化，再按宿主视角调用导出的 C ABI。这类验收需要真实的
//! Wasm 运行时，不适合作为 Boost 的常规测试依赖，故按需手动运行。
//!
//! ```text
//! rustup target add wasm32-unknown-unknown
//! cargo build -p hardware-wasm --target wasm32-unknown-unknown --release
//! cargo test --manifest-path tools/wasm-contract/Cargo.toml
//! ```
#![warn(missing_docs)]
